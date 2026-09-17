//! The compositor's thread: the loop that drives the Wayland clients.

use std::{
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
    channel::{Channel, Event as ChannelEvent, Sender},
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use smithay::{reexports::wayland_server::Display, wayland::socket::ListeningSocketSource};

use crate::{
    Error,
    dmabuf::RenderNode,
    logging,
    wayland::{
        message::{Command, Event},
        state::{Compositor, REFRESH_MILLIHZ},
    },
};

/// The frame cap. A client that draws faster than a terminal can show waits
/// here, and frames it draws in the meantime are dropped, not queued.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000_000 / REFRESH_MILLIHZ as u64);

/// Run the compositor until it is told to stop, and report whether it started.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the thread outlives whoever started it, so it owns its end of the channel rather than borrowing it"
)]
pub(super) fn run(
    socket: ListeningSocketSource,
    nodes: &[RenderNode],
    events: Sender<Event>,
    commands: Channel<Command>,
    ready: SyncSender<Result<(), Error>>,
) {
    let (mut event_loop, mut compositor) = match Loop::start(socket, nodes, events, commands) {
        Ok(started) => started,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    if let Err(error) = event_loop.run(None, &mut compositor, |_| {}) {
        tracing::error!(?error, "the compositor loop stopped");
    }
    compositor.flush();
}

/// The compositor's loop: the display, the state it drives, and the frame cap.
struct Loop {
    display: Display<Compositor>,
    state: Compositor,
    handle: LoopHandle<'static, Self>,
    signal: Option<LoopSignal>,
    frame_scheduled: bool,
    last_frame_started: Option<Instant>,
    frames: FrameStats,
}

impl Loop {
    /// Build the display and its state, and wire up everything that feeds the
    /// loop.
    fn start(
        socket: ListeningSocketSource,
        nodes: &[RenderNode],
        events: Sender<Event>,
        commands: Channel<Command>,
    ) -> Result<(EventLoop<'static, Self>, Self), Error> {
        let display: Display<Compositor> = Display::new().map_err(Error::Display)?;
        let state = Compositor::new(&display.handle(), nodes, events)?;

        let event_loop: EventLoop<'static, Self> = EventLoop::try_new()?;
        let handle = event_loop.handle();
        let mut compositor = Self {
            display,
            state,
            handle: handle.clone(),
            signal: None,
            frame_scheduled: false,
            last_frame_started: None,
            frames: FrameStats::default(),
        };
        compositor.signal = Some(event_loop.get_signal());
        watch_socket(socket, &handle)?;
        compositor.watch_display(&handle)?;
        watch_commands(commands, &handle)?;
        Ok((event_loop, compositor))
    }

    /// A Wayland client has said something.
    fn watch_display(&mut self, handle: &LoopHandle<'static, Self>) -> Result<(), Error> {
        let poll_fd =
            rustix::io::dup(self.display.backend().poll_fd()).map_err(|error| Error::Watch {
                source: "the display socket",
                cause: error.into(),
            })?;
        handle
            .insert_source(
                Generic::new(poll_fd, Interest::READ, Mode::Level),
                |_, _, compositor: &mut Self| {
                    let dispatched = compositor.display.dispatch_clients(&mut compositor.state);
                    if let Err(error) = dispatched {
                        tracing::warn!(?error, "dispatching to clients failed");
                    }
                    compositor.settled();
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|refused| watch("the display", refused))?;
        Ok(())
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Attach {
                pane,
                show,
                capabilities,
            } => self.state.attach_view(pane, show, &capabilities),
            Command::Detach { pane } => self.state.detach_view(pane),
            Command::Resized { pane, capabilities } => {
                self.state.resize_view(pane, &capabilities);
            }
            Command::Input { pane, input } => self.state.input(pane, input),
            Command::Close => self.state.close_windows(),
            Command::Frame { pane, frame, tiles } => self.state.recycle(pane, frame, tiles),
        }
    }

    /// Draw a frame for every pane that is due, then flush what the clients
    /// were told.
    fn present_frame(&mut self) {
        let started = Instant::now();
        let tiles = self.state.present();
        self.last_frame_started = Some(started);
        self.frames.record(tiles, started.elapsed());
        self.flush();
    }

    /// Everything has been handled: tell the clients, and draw if anything is
    /// waiting to be drawn.
    fn settled(&mut self) {
        self.flush();
        self.schedule_frame();
    }

    fn flush(&mut self) {
        if let Err(error) = self.display.flush_clients() {
            tracing::warn!(?error, "flushing to clients failed");
        }
    }

    fn stop(&self) {
        if let Some(signal) = &self.signal {
            signal.stop();
        }
    }

    /// Draw a frame once the frame cap allows one, if a pane is waiting for it.
    fn schedule_frame(&mut self) {
        if self.frame_scheduled || !self.state.any_due() {
            return;
        }
        self.frame_scheduled = true;
        let deadline = frame_deadline(Instant::now(), self.last_frame_started);
        if let Err(error) = self.handle.insert_source(
            Timer::from_deadline(deadline),
            move |_, (), compositor: &mut Self| {
                compositor.frame_scheduled = false;
                compositor.present_frame();
                compositor.schedule_frame();
                TimeoutAction::Drop
            },
        ) {
            self.frame_scheduled = false;
            tracing::warn!(?error, "could not schedule a frame");
        }
    }
}

/// A Wayland client has connected.
fn watch_socket(
    socket: ListeningSocketSource,
    handle: &LoopHandle<'static, Loop>,
) -> Result<(), Error> {
    handle
        .insert_source(socket, |stream, (), compositor: &mut Loop| {
            if let Err(error) = compositor.state.insert_client(stream) {
                tracing::warn!(?error, "could not adopt a client");
            }
        })
        .map_err(|refused| watch("the Wayland socket", refused))?;
    Ok(())
}

/// The server has said something.
fn watch_commands(
    commands: Channel<Command>,
    handle: &LoopHandle<'static, Loop>,
) -> Result<(), Error> {
    handle
        .insert_source(commands, |event, (), compositor: &mut Loop| {
            match event {
                ChannelEvent::Msg(command) => compositor.command(command),
                // The server dropped its end: it is going away.
                ChannelEvent::Closed => compositor.stop(),
            }
            compositor.settled();
        })
        .map_err(|refused| watch("the server", refused))?;
    Ok(())
}

/// A source that the event loop refused, named so that the failure can be told
/// from the others.
fn watch<T: std::fmt::Debug>(source: &'static str, refused: T) -> Error {
    Error::Watch {
        source,
        cause: format!("{refused:?}").into(),
    }
}

/// What the frames of one second cost this thread, logged so that a slow frame
/// rate is attributed and not guessed at. This is the thread that composes, so
/// time spent here is time a pane waits for its next frame.
///
/// The presenter keeps its own count of the compressing and the writing.
#[derive(Debug, Default)]
struct FrameStats {
    report: logging::Report,
    frames: u32,
    tiles: u64,
    compose: Duration,
}

impl FrameStats {
    fn record(&mut self, tiles: usize, compose: Duration) {
        self.frames += 1;
        self.tiles += tiles as u64;
        self.compose += compose;

        let Some(seconds) = self.report.due() else {
            return;
        };
        let frames = f64::from(self.frames);
        tracing::debug!(
            fps = frames / seconds,
            tiles = self.tiles / u64::from(self.frames),
            compose_ms = self.compose.as_secs_f64() * 1e3 / frames,
            "frames composed"
        );
        *self = Self {
            report: self.report,
            ..Self::default()
        };
    }
}

/// The earliest deadline that holds the frame cap and adds no idle latency.
///
/// `last_frame_started` is when the previous frame began. A frame that takes
/// longer than the interval is late, but the next frame is not pushed out by
/// the work that the late frame already paid for.
fn frame_deadline(now: Instant, last_frame_started: Option<Instant>) -> Instant {
    last_frame_started
        .and_then(|started| started.checked_add(FRAME_INTERVAL))
        .map_or(now, |deadline| deadline.max(now))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FRAME_INTERVAL, frame_deadline};

    #[test]
    fn an_idle_compositor_schedules_the_next_frame_immediately() {
        let now = Instant::now();
        let old = now
            .checked_sub(FRAME_INTERVAL + Duration::from_millis(1))
            .expect("the monotonic clock has advanced past startup");
        assert_eq!(frame_deadline(now, None), now);
        assert_eq!(frame_deadline(now, Some(old)), now);
    }

    #[test]
    fn an_active_compositor_preserves_the_frame_cap() {
        let started = Instant::now();
        let now = started + Duration::from_millis(1);
        assert_eq!(frame_deadline(now, Some(started)), started + FRAME_INTERVAL);
    }

    #[test]
    fn a_slow_frame_does_not_push_the_next_one_out() {
        // A frame that took longer than the interval has paid for its own time.
        // The next frame goes as soon as there is something to show, and not a
        // further interval after the work finished.
        let now = Instant::now();
        let started = now
            .checked_sub(FRAME_INTERVAL + Duration::from_millis(10))
            .expect("the monotonic clock has advanced past startup");
        assert_eq!(frame_deadline(now, Some(started)), now);
    }
}
