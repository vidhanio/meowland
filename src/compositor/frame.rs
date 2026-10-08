//! Independent Wayland callback clock and coalesced, single-in-flight pane
//! frames.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use super::{
    DISPLAY_POLL_INTERVAL, Event, FRAME_INTERVAL, State, SurfaceAttributes, render_frame,
    surface_stack, with_states,
};

pub(super) struct PaneState {
    pub(super) window: u64,
    pub(super) width: u32,
    pub(super) height: u32,
    /// At most one frame awaits an ack; subsequent commits coalesce in `dirty`.
    pub(super) in_flight: bool,
    pub(super) dirty: bool,
    next_frame: Instant,
    shown: Vec<u8>,
    /// An undrawn frame cannot be used as the basis for a row-band diff.
    shown_stale: bool,
}

impl PaneState {
    pub(super) fn new(window: u64, width: u32, height: u32) -> Self {
        Self {
            window,
            width,
            height,
            in_flight: false,
            dirty: false,
            next_frame: Instant::now(),
            shown: Vec::new(),
            shown_stale: false,
        }
    }
}

/// Cap the unpollable command wait by the next frame or display I/O deadline.
pub(super) fn next_wakeup(state: &State, now: Instant) -> Duration {
    let mut wait = DISPLAY_POLL_INTERVAL;
    for window in state.windows.values() {
        wait = wait.min(window.callback_due.saturating_duration_since(now));
    }
    for pane in &state.pending {
        if let Some(entry) = state.panes.get(pane)
            && !entry.in_flight
        {
            wait = wait.min(entry.next_frame.saturating_duration_since(now));
        }
    }
    wait
}

/// Run Wayland callbacks per window at ~60Hz, independently of terminal acks.
/// An unresponsive pane cannot stall its client's drawing or event handling.
pub(super) fn frame_callbacks(state: &mut State) {
    let now = Instant::now();
    for window in state.windows.values_mut() {
        if now < window.callback_due {
            continue;
        }
        // Skip missed ticks instead of bursting callbacks or drifting the
        // clock.
        let late = now.duration_since(window.callback_due).as_nanos();
        let ticks = late / FRAME_INTERVAL.as_nanos() + 1;
        window.callback_due += FRAME_INTERVAL * u32::try_from(ticks).unwrap_or(u32::MAX);
        let root = window.surface.wl_surface().clone();
        let time = state.started.elapsed().as_millis() as u32;
        for (surface, _) in surface_stack(&root) {
            with_states(&surface, |states| {
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
    }
}

/// A pane ack frees only its presentation slot, not Wayland callbacks.
pub(super) fn pane_ack(state: &mut State, pane: u64, drawn: bool) {
    let Some(entry) = state.panes.get_mut(&pane) else {
        return;
    };
    if !entry.in_flight {
        return;
    }
    entry.in_flight = false;
    entry.shown_stale = !drawn;
    if std::mem::take(&mut entry.dirty) || !drawn {
        state.mark_dirty(pane);
    }
}

/// Coalesce pending changes, sending at most one frame per pane per tick.
pub(super) fn dispatch_frames(state: &mut State) {
    if state.pending.is_empty() {
        return;
    }
    let now = Instant::now();
    let due: Vec<u64> = state
        .pending
        .iter()
        .copied()
        .filter(|pane| {
            state
                .panes
                .get(pane)
                .is_some_and(|entry| !entry.in_flight && now >= entry.next_frame)
        })
        .collect();
    for pane in due {
        state.pending.remove(&pane);
        if !render_frame(state, pane) {
            continue;
        }
        let Some(entry) = state.panes.get_mut(&pane) else {
            continue;
        };
        entry.next_frame = now + FRAME_INTERVAL;
        let stride = entry.width as usize * 3;
        let rows = if entry.shown_stale {
            Some(0..entry.height as usize)
        } else {
            changed_rows(&entry.shown, &state.scratch, stride)
        };
        let Some(rows) = rows else {
            continue;
        };
        let start = rows.start * stride;
        let end = rows.end * stride;
        // The event buffer must not alias the pane's retained diff baseline.
        let band = state.scratch[start..end].to_vec();
        entry.shown.resize(state.scratch.len(), 0);
        entry.shown[start..end].copy_from_slice(&band);
        entry.in_flight = true;
        let _ = state.events.send(Event::Frame {
            pane,
            width: entry.width,
            height: entry.height,
            y: rows.start as u32,
            rgb: band,
        });
    }
}

fn changed_rows(shown: &[u8], buffer: &[u8], stride: usize) -> Option<Range<usize>> {
    if stride == 0 {
        return None;
    }
    let rows = buffer.len() / stride;
    if shown.len() != buffer.len() {
        return Some(0..rows);
    }
    let differs = |row: usize| {
        shown[row * stride..(row + 1) * stride] != buffer[row * stride..(row + 1) * stride]
    };
    let first = (0..rows).find(|row| differs(*row))?;
    let last = (first..rows)
        .rev()
        .find(|row| differs(*row))
        .unwrap_or(first);
    Some(first..last + 1)
}
