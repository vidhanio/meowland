//! Independent Wayland callback clocks and damage-aware retained pane frames.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use meowland_core::{latency::Latency, pixels::FrameSize};
use meowland_protocol::{FrameAck, FrameBand, FrameTransport, SharedObject};
use meowland_render::{Changed, Renderer, Scene, Snapshot, SurfaceDamage};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{Compositor, Event, PaneId, ServerToPane};

mod publication;

use publication::Publication;

/// Where a pane's next frame stands.
#[derive(Debug)]
enum Presentation {
    /// The pane shows the latest composition.
    Clean,
    /// Content changed at `since` and a frame is waiting for dispatch.
    Dirty { since: Instant },
    /// A frame is awaiting its acknowledgement.
    InFlight {
        /// When the content the frame shows first changed.
        changed: Instant,
        sent: Instant,
        /// The shared object the frame references, kept alive until the pane
        /// read it.
        _shared: Option<SharedObject>,
        /// When content changed again since the frame was sent.
        pending: Option<Instant>,
    },
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

/// Rectangles at least this wide, as a percentage of the frame width, are sent
/// as full rows.
///
/// Narrow rectangles save bytes and terminal-side work, but wide ones gain
/// little: zlib finds fewer cross-row matches, the terminal copies a row per
/// row instead of a block, and the metadata overhead is unchanged.
const FULL_ROW_PERCENT: u32 = 75;

/// Columns transferred for a changed rectangle.
///
/// Widening nearly full-width rectangles to whole rows keeps the payload shape
/// the terminal treats as a plain row range. The same columns are estimated by
/// the shared-transport decision, so both agree on the bytes involved.
fn transfer_columns(rect: &Changed, size: FrameSize) -> Range<u32> {
    let width = rect.columns.end - rect.columns.start;
    if width * 100 >= size.width() * FULL_ROW_PERCENT {
        0..size.width()
    } else {
        rect.columns.clone()
    }
}

pub(crate) struct PaneState {
    pub size: FrameSize,
    presentation: Presentation,
    scene: Scene,
    /// Size of the last complete frame the pane retained; any other size
    /// needs a complete frame.
    sent_size: Option<FrameSize>,
    pub(super) transport: FrameTransport,
    stats: FrameStats,
}

impl PaneState {
    pub(crate) fn new(size: FrameSize) -> Self {
        Self {
            size,
            presentation: Presentation::Clean,
            scene: Scene::default(),
            sent_size: None,
            transport: FrameTransport::Inline,
            stats: FrameStats::new(),
        }
    }

    fn schedule(&mut self, changed: Instant) {
        match &mut self.presentation {
            Presentation::Clean => self.presentation = Presentation::Dirty { since: changed },
            Presentation::Dirty { since } => *since = (*since).min(changed),
            Presentation::InFlight { pending, .. } => {
                *pending = Some(pending.map_or(changed, |previous| previous.min(changed)));
            }
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.scene.invalidate();
        self.schedule(Instant::now());
    }

    /// Drop pending work while the pane shows no window. An in-flight frame
    /// still waits for its acknowledgement; the pane renegotiates its
    /// transport when presentation restarts, and the next frame is complete.
    pub(crate) fn unbind(&mut self) {
        self.scene.invalidate();
        self.sent_size = None;
        self.transport = FrameTransport::Inline;
        match &mut self.presentation {
            Presentation::Dirty { .. } => self.presentation = Presentation::Clean,
            Presentation::InFlight { pending, .. } => *pending = None,
            Presentation::Clean => {}
        }
    }

    pub(crate) fn damage(&mut self, surface: &WlSurface, damage: SurfaceDamage, changed: Instant) {
        self.scene.damage(surface, damage);
        self.schedule(changed);
    }

    pub(crate) fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        let (changed, sent, pending) =
            match std::mem::replace(&mut self.presentation, Presentation::Clean) {
                Presentation::InFlight {
                    changed,
                    sent,
                    pending,
                    ..
                } => (changed, sent, pending),
                presentation => {
                    self.presentation = presentation;
                    return;
                }
            };
        self.stats.ack(pane, changed, sent);
        if let Some(since) = pending {
            self.presentation = Presentation::Dirty { since };
        }
        if ack == FrameAck::Resync {
            self.sent_size = None;
            self.mark_dirty();
        }
    }

    pub(crate) const fn due(&self) -> bool {
        matches!(self.presentation, Presentation::Dirty { .. })
    }

    fn frame(
        &mut self,
        renderer: &mut Renderer,
        root: Option<&WlSurface>,
        snapshots: &std::collections::HashMap<WlSurface, Snapshot>,
        scale: f64,
    ) -> Option<ServerToPane> {
        let started = Instant::now();
        let changed = match std::mem::replace(&mut self.presentation, Presentation::Clean) {
            Presentation::Dirty { since } => since,
            Presentation::Clean => started,
            presentation @ Presentation::InFlight { .. } => {
                self.presentation = presentation;
                return None;
            }
        };
        let mut publication = Publication::new(self.size, self.transport, self.sent_size);
        let rect = renderer.update(
            &mut self.scene,
            root,
            snapshots,
            self.size,
            scale,
            |changed, ready| publication.changed(changed, ready),
        );
        self.transport = publication.transport();
        let rect = if self.sent_size == Some(self.size) {
            rect?
        } else {
            Changed {
                columns: 0..self.size.width(),
                rows: 0..self.size.height(),
            }
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
        let shared = publication.finish(self.scene.pixels());
        self.transport = publication.transport();
        let message = shared.as_ref().map_or_else(
            || self.band(&rect),
            |object| ServerToPane::SharedFrame(*object.frame()),
        );
        self.stats.publish.record(streamed + publishing.elapsed());
        self.presentation = Presentation::InFlight {
            changed,
            sent: Instant::now(),
            _shared: shared,
            pending: None,
        };
        Some(message)
    }

    fn band(&self, rect: &Changed) -> ServerToPane {
        let columns = transfer_columns(rect, self.size);
        let (x, width) = (columns.start, columns.end - columns.start);
        let stride = self.size.rgb_stride();
        let row_bytes = usize::try_from(width).expect("validated width fits usize") * 3;
        let rows = rect.rows.end - rect.rows.start;
        let start = usize::try_from(rect.rows.start).expect("row offset fits validated height")
            * stride
            + usize::try_from(x).expect("validated column fits usize") * 3;
        let mut rgb = Vec::with_capacity(
            row_bytes * usize::try_from(rows).expect("validated height fits usize"),
        );
        let pixels = self.scene.pixels();
        for row in 0..rows {
            let offset =
                start + usize::try_from(row).expect("row offset fits validated height") * stride;
            rgb.extend_from_slice(&pixels[offset..offset + row_bytes]);
        }
        ServerToPane::Frame(
            FrameBand::new(self.size, x, rect.rows.start, width, rgb)
                .expect("composed rectangle fits its frame"),
        )
    }
}

impl Compositor {
    /// Wait for frame or activation deadlines; transfers wake through readiness
    /// sources.
    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if self.attachments.due_frames() > 0 {
            return Some(Duration::ZERO);
        }
        self.activation_deadline()
            .into_iter()
            .chain(self.callback_deadline())
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Earliest paced deadline of a window waiting for a frame callback.
    fn callback_deadline(&self) -> Option<Instant> {
        self.windows
            .by_id
            .values()
            .filter(|window| window.callback_pending)
            .map(|window| window.callback_due)
            .min()
    }

    /// Pace visible clients at the configured refresh rate and hidden clients
    /// at one Hz.
    pub(crate) fn frame_callbacks(&mut self) {
        let now = Instant::now();
        let time = self.started.elapsed();
        for (&id, window) in &mut self.windows.by_id {
            if !window.callback_pending || now < window.callback_due {
                continue;
            }
            let interval = if self.attachments.visible(id) && self.windows.catalog.drawable(id) {
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

    /// Queue a frame callback at the window's paced deadline.
    pub(crate) fn request_callback(&mut self, window: super::WindowId) {
        if let Some(entry) = self.windows.by_id.get_mut(&window) {
            entry.callback_pending = true;
        }
    }

    pub(crate) fn resume_callbacks(&mut self, window: super::WindowId) {
        if self.attachments.visible(window)
            && let Some(window) = self.windows.by_id.get_mut(&window)
        {
            window.callback_due = window
                .callback_due
                .min(Instant::now() + self.refresh_rate.interval());
        }
    }

    pub(crate) fn dispatch_frames(&mut self) {
        if self.attachments.due_frames() == 0 {
            return;
        }
        let renderer = &mut self.renderer;
        let windows = &self.windows;
        let scale = self.scale.factor();
        let events = &mut self.events;
        self.attachments
            .for_each_presentation_mut(|pane, window, presentation| {
                if !presentation.due() {
                    return;
                }
                let Some(window) = window else {
                    presentation.unbind();
                    return;
                };
                let root = Some(window)
                    .filter(|&id| windows.catalog.drawable(id))
                    .and_then(|id| windows.by_id.get(&id))
                    .map(|window| window.surface.wl_surface());
                if let Some(message) =
                    presentation.frame(renderer, root, &windows.surface_cache.snapshots, scale)
                {
                    events.push(Event { pane, message });
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use meowland_protocol::{Follow, Selector, Target};

    use super::*;
    use crate::{Attachments, WindowId};

    fn pane() -> PaneId {
        PaneId::from(1)
    }

    fn shown_pane() -> Attachments {
        let mut attachments = Attachments::default();
        attachments.insert(
            pane(),
            Target::Selector(Selector::AppId("app".into())),
            Follow::First,
            FrameSize::new(4, 2).expect("test size is valid"),
        );
        attachments.show(pane(), WindowId::from(1));
        attachments
    }

    fn send_due_frames(attachments: &mut Attachments, renderer: &mut Renderer) -> usize {
        let mut sent = 0;
        attachments.for_each_presentation_mut(|_, _, presentation| {
            if presentation.due()
                && presentation
                    .frame(renderer, None, &HashMap::new(), 1.0)
                    .is_some()
            {
                sent += 1;
            }
        });
        sent
    }

    fn presentation(attachments: &Attachments) -> &Presentation {
        &attachments
            .get(pane())
            .expect("test pane is attached")
            .presentation
            .presentation
    }

    fn change(attachments: &mut Attachments) {
        attachments.for_each_showing_mut(WindowId::from(1), |_, presentation| {
            presentation.mark_dirty();
        });
    }

    #[test]
    fn retained_ack_without_changes_leaves_the_pane_clean() {
        let mut renderer = Renderer::default();
        let mut attachments = shown_pane();
        assert_eq!(send_due_frames(&mut attachments, &mut renderer), 1);
        assert!(matches!(
            presentation(&attachments),
            Presentation::InFlight { .. }
        ));

        attachments.ack(pane(), FrameAck::Retained);

        assert!(matches!(presentation(&attachments), Presentation::Clean));
        assert_eq!(attachments.due_frames(), 0);
    }

    #[test]
    fn changes_during_flight_wait_for_the_retained_ack() {
        let mut renderer = Renderer::default();
        let mut attachments = shown_pane();
        send_due_frames(&mut attachments, &mut renderer);

        change(&mut attachments);
        assert!(matches!(
            presentation(&attachments),
            Presentation::InFlight { .. }
        ));
        assert_eq!(attachments.due_frames(), 0);

        attachments.ack(pane(), FrameAck::Retained);

        assert!(matches!(
            presentation(&attachments),
            Presentation::Dirty { .. }
        ));
        assert_eq!(attachments.due_frames(), 1);
    }

    #[test]
    fn resync_ack_schedules_a_full_frame() {
        let mut renderer = Renderer::default();
        let mut attachments = shown_pane();
        send_due_frames(&mut attachments, &mut renderer);

        attachments.ack(pane(), FrameAck::Resync);

        assert!(matches!(
            presentation(&attachments),
            Presentation::Dirty { .. }
        ));
        assert_eq!(attachments.due_frames(), 1);
        assert_eq!(
            attachments
                .get(pane())
                .expect("test pane is attached")
                .presentation
                .sent_size,
            None
        );
    }

    #[test]
    fn acks_without_a_frame_in_flight_are_ignored() {
        let mut renderer = Renderer::default();
        let mut attachments = shown_pane();
        for ack in [FrameAck::Retained, FrameAck::Resync] {
            attachments.ack(pane(), ack);
            assert!(matches!(
                presentation(&attachments),
                Presentation::Dirty { .. }
            ));
            assert_eq!(attachments.due_frames(), 1);
        }

        send_due_frames(&mut attachments, &mut renderer);
        attachments.ack(pane(), FrameAck::Retained);
        for ack in [FrameAck::Retained, FrameAck::Resync] {
            attachments.ack(pane(), ack);
            assert!(matches!(presentation(&attachments), Presentation::Clean));
            assert_eq!(attachments.due_frames(), 0);
        }
    }
}
