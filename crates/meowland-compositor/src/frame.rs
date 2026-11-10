//! Independent Wayland callback clocks and damage-aware retained pane frames.

use std::{
    fs::File,
    io::{self, Write as _},
    ops::Range,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameBand, SharedObject};
use meowland_render::{Changed, Renderer, Scene, Snapshot, SurfaceDamage};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{Compositor, Event, ServerToPane};

/// Publish whole frames through SHM when the changed rectangle covers at least
/// this fraction of the frame area.
///
/// A shared frame transfers every pixel once; an inline rectangle transfers
/// only the changed columns for the changed rows. Below a quarter of the frame
/// the rectangle is smaller, even after zlib, and skips the publication write.
const SHARED_AREA_DIVISOR: u64 = 4;

/// Where a pane's next frame stands.
#[derive(Debug)]
enum Presentation {
    /// The pane shows the latest composition.
    Clean,
    /// Content changed and a frame is waiting for dispatch.
    Dirty,
    /// A frame is awaiting its acknowledgement.
    InFlight {
        /// The shared object the frame references, kept alive until the pane
        /// read it. The open file keeps its pages allocated, so freeing them
        /// never lands on the terminal's critical path when it closes them.
        _shared: Option<(SharedObject, File)>,
        /// Content changed again since the frame was sent.
        dirty: bool,
    },
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
/// the terminal treats as a plain row range.
fn transfer_columns(rect: &Changed, size: FrameSize) -> Range<u32> {
    let width = rect.columns.end - rect.columns.start;
    if width * 100 >= size.width() * FULL_ROW_PERCENT {
        0..size.width()
    } else {
        rect.columns.clone()
    }
}

/// Whether a whole shared frame is cheaper than one inline rectangle.
fn prefers_shared(rect: &Changed, size: FrameSize) -> bool {
    let columns = transfer_columns(rect, size);
    let width = u64::from(columns.end - columns.start);
    let rows = u64::from(rect.rows.end - rect.rows.start);
    width * rows * SHARED_AREA_DIVISOR >= u64::from(size.width()) * u64::from(size.height())
}

/// A whole frame written into a fresh shared object while the renderer
/// composes it, so each strip is copied while it is still in cache.
struct Publication {
    object: SharedObject,
    file: File,
    written: usize,
}

/// Continue the frame's publication with the composed prefix `ready`, first
/// creating an object for a frame of size `create` if none exists. A failure
/// stops publication for the frame.
fn publish(
    publication: &mut io::Result<Option<Publication>>,
    create: Option<FrameSize>,
    ready: &[u8],
) {
    let Ok(current) = publication else {
        return;
    };
    let next = match (current.take(), create) {
        (Some(publication), _) => Ok(publication),
        (None, Some(size)) => SharedObject::create(size).map(|(object, file)| Publication {
            object,
            file,
            written: 0,
        }),
        (None, None) => return,
    };
    *publication = next.and_then(|mut publication| {
        publication.file.write_all(&ready[publication.written..])?;
        publication.written = ready.len();
        Ok(Some(publication))
    });
}

pub(crate) struct PaneState {
    pub size: FrameSize,
    presentation: Presentation,
    scene: Scene,
    /// Size of the last complete frame the pane holds; any other size needs
    /// a complete frame.
    sent_size: Option<FrameSize>,
    /// Large frames go out as shared objects until the pane reports that its
    /// terminal cannot read them, or publishing one fails.
    pub(super) shared: bool,
}

impl PaneState {
    pub(crate) fn new(size: FrameSize) -> Self {
        Self {
            size,
            presentation: Presentation::Clean,
            scene: Scene::default(),
            sent_size: None,
            shared: true,
        }
    }

    fn schedule(&mut self) {
        match &mut self.presentation {
            Presentation::Clean => self.presentation = Presentation::Dirty,
            Presentation::Dirty => {}
            Presentation::InFlight { dirty, .. } => *dirty = true,
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.scene.invalidate();
        self.schedule();
    }

    /// Drop pending work while the pane shows no window. An in-flight frame
    /// still waits for its acknowledgement, and the next frame is complete.
    pub(crate) fn unbind(&mut self) {
        self.scene.invalidate();
        self.sent_size = None;
        match &mut self.presentation {
            Presentation::Dirty => self.presentation = Presentation::Clean,
            Presentation::InFlight { dirty, .. } => *dirty = false,
            Presentation::Clean => {}
        }
    }

    pub(crate) fn damage(&mut self, surface: &WlSurface, damage: SurfaceDamage) {
        self.scene.damage(surface, damage);
        self.schedule();
    }

    /// Release the in-flight frame, freeing the pane for the next one.
    pub(crate) fn ack(&mut self) {
        if let Presentation::InFlight { dirty, .. } = self.presentation {
            self.presentation = if dirty {
                Presentation::Dirty
            } else {
                Presentation::Clean
            };
        }
    }

    /// Send the next frame whole, because the pane holds no image to edit.
    pub(crate) fn keyframe(&mut self) {
        self.sent_size = None;
        self.schedule();
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
        if matches!(self.presentation, Presentation::InFlight { .. }) {
            return None;
        }
        self.presentation = Presentation::Clean;
        let size = self.size;
        let shared = self.shared;
        let whole = self.sent_size != Some(size);
        let mut publication = Ok(None);
        let rect = renderer.update(
            &mut self.scene,
            root,
            snapshots,
            size,
            scale,
            |changed, ready| {
                let start = shared && (whole || changed.is_some_and(|c| prefers_shared(c, size)));
                publish(&mut publication, start.then_some(size), ready);
            },
        );
        let rect = if whole {
            Changed {
                columns: 0..size.width(),
                rows: 0..size.height(),
            }
        } else {
            rect?
        };
        self.sent_size = Some(size);
        let start = shared && (whole || prefers_shared(&rect, size));
        publish(&mut publication, start.then_some(size), self.scene.pixels());
        let shared = publication
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "shared-memory publication failed; using RGB bands");
                self.shared = false;
                None
            })
            .map(|publication| (publication.object, publication.file));
        let message = shared.as_ref().map_or_else(
            || self.band(&rect),
            |(object, _)| ServerToPane::SharedFrame(*object.frame()),
        );
        self.presentation = Presentation::InFlight {
            _shared: shared,
            dirty: false,
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
        if self.frames_due() {
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
            let interval =
                if !self.attachments.showing(id).is_empty() && self.windows.catalog.drawable(id) {
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
        if !self.attachments.showing(window).is_empty()
            && let Some(window) = self.windows.by_id.get_mut(&window)
        {
            window.callback_due = window
                .callback_due
                .min(Instant::now() + self.refresh_rate.interval());
        }
    }

    pub(crate) fn dispatch_frames(&mut self) {
        if !self.frames_due() {
            return;
        }
        let renderer = &mut self.renderer;
        let windows = &self.windows;
        let scale = self.scale.factor();
        let events = &mut self.events;
        for (pane, window, state) in self.attachments.iter_mut() {
            if !state.due() {
                continue;
            }
            let Some(window) = window else {
                state.unbind();
                continue;
            };
            let root = Some(window)
                .filter(|&id| windows.catalog.drawable(id))
                .and_then(|id| windows.by_id.get(&id))
                .map(|window| window.surface.wl_surface());
            if let Some(message) =
                state.frame(renderer, root, &windows.surface_cache.snapshots, scale)
            {
                events.push(Event { pane, message });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn shown() -> PaneState {
        let mut state = PaneState::new(FrameSize::new(4, 2).expect("test size is valid"));
        state.mark_dirty();
        state
    }

    fn send(state: &mut PaneState, renderer: &mut Renderer) -> Option<ServerToPane> {
        state
            .due()
            .then(|| state.frame(renderer, None, &HashMap::new(), 1.0))
            .flatten()
    }

    #[test]
    fn ack_without_changes_leaves_the_pane_clean() {
        let mut renderer = Renderer::default();
        let mut state = shown();
        assert!(send(&mut state, &mut renderer).is_some());
        assert!(matches!(state.presentation, Presentation::InFlight { .. }));

        state.ack();

        assert!(matches!(state.presentation, Presentation::Clean));
        assert!(!state.due());
    }

    #[test]
    fn changes_during_flight_wait_for_the_ack() {
        let mut renderer = Renderer::default();
        let mut state = shown();
        send(&mut state, &mut renderer);

        state.mark_dirty();
        assert!(matches!(state.presentation, Presentation::InFlight { .. }));
        assert!(!state.due());

        state.ack();

        assert!(matches!(state.presentation, Presentation::Dirty));
        assert!(state.due());
    }

    #[test]
    fn a_keyframe_request_sends_a_whole_frame_after_the_ack() {
        let mut renderer = Renderer::default();
        let mut state = shown();
        state.shared = false;
        send(&mut state, &mut renderer);
        state.ack();

        state.keyframe();
        state.keyframe();
        assert!(state.due());
        send(&mut state, &mut renderer);
        state.keyframe();
        assert!(!state.due(), "the frame in flight is unacknowledged");
        state.ack();

        let message = send(&mut state, &mut renderer);
        assert!(
            matches!(&message, Some(ServerToPane::Frame(band)) if band.is_whole()),
            "{message:?}"
        );
    }

    #[test]
    fn acks_without_a_frame_in_flight_are_ignored() {
        let mut renderer = Renderer::default();
        let mut state = shown();
        state.ack();
        assert!(matches!(state.presentation, Presentation::Dirty));

        send(&mut state, &mut renderer);
        state.ack();
        state.ack();
        assert!(matches!(state.presentation, Presentation::Clean));
        assert!(!state.due());
    }
}
