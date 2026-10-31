//! Line protocol between the harness and the workload over a Unix socket.
//!
//! Every event carries a `CLOCK_MONOTONIC` timestamp from [`now`], the same
//! clock the endpoint stamps consumed frames with.

use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context as _, bail};
use rustix::time::{ClockId, clock_gettime};

/// Time since an arbitrary boot-time epoch on `CLOCK_MONOTONIC`, comparable
/// across processes.
pub fn now() -> Duration {
    let time = clock_gettime(ClockId::Monotonic);
    Duration::from_secs(time.tv_sec.unsigned_abs())
        + Duration::from_nanos(time.tv_nsec.unsigned_abs())
}

/// A request from the harness to the workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Paint and commit this frame.
    Commit(u64),
    /// Commit under ID `frame` a buffer holding `content`'s pixels.
    Repeat {
        /// ID reported for the commit.
        frame: u64,
        /// Frame whose pixels are painted.
        content: u64,
    },
    /// Respond to the next `a` key press by committing this frame.
    Arm(u64),
    /// Exit cleanly.
    Stop,
}

impl fmt::Display for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(frame) => write!(formatter, "commit {frame}"),
            Self::Repeat { frame, content } => write!(formatter, "repeat {frame} {content}"),
            Self::Arm(frame) => write!(formatter, "arm {frame}"),
            Self::Stop => formatter.write_str("stop"),
        }
    }
}

impl FromStr for Command {
    type Err = anyhow::Error;

    fn from_str(line: &str) -> anyhow::Result<Self> {
        let mut words = line.split_whitespace();
        let kind = words.next().context("empty command")?;
        let mut number =
            || -> anyhow::Result<u64> { Ok(words.next().context("missing frame ID")?.parse()?) };
        let command = match kind {
            "commit" => Self::Commit(number()?),
            "repeat" => Self::Repeat {
                frame: number()?,
                content: number()?,
            },
            "arm" => Self::Arm(number()?),
            "stop" => Self::Stop,
            _ => bail!("unknown command {kind:?}"),
        };
        if words.next().is_some() {
            bail!("trailing words in command {line:?}");
        }
        Ok(command)
    }
}

/// What a workload [`Event`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// The workload's process ID, carried as the event ID.
    Pid,
    /// The surface has keyboard focus.
    Ready,
    /// An [`Command::Arm`] took effect.
    Armed,
    /// The frame was committed; the timestamp precedes `wl_surface.commit`.
    Commit,
    /// The armed key press arrived.
    Input,
    /// The compositor released the buffer that carried the frame.
    Release,
}

impl EventKind {
    const ALL: [Self; 6] = [
        Self::Pid,
        Self::Ready,
        Self::Armed,
        Self::Commit,
        Self::Input,
        Self::Release,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::Ready => "ready",
            Self::Armed => "armed",
            Self::Commit => "commit",
            Self::Input => "input",
            Self::Release => "release",
        }
    }
}

/// A timestamped report from the workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// What happened.
    pub kind: EventKind,
    /// The frame it concerns, or the process ID for [`EventKind::Pid`].
    pub id: u64,
    /// When it happened, from [`now`].
    pub at: Duration,
}

impl fmt::Display for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} {}",
            self.kind.name(),
            self.id,
            self.at.as_nanos()
        )
    }
}

impl FromStr for Event {
    type Err = anyhow::Error;

    fn from_str(line: &str) -> anyhow::Result<Self> {
        let parse = || -> Option<Self> {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let kind = EventKind::ALL
                .into_iter()
                .find(|kind| kind.name() == name)?;
            let id = words.next()?.parse().ok()?;
            let at = Duration::from_nanos(words.next()?.parse().ok()?);
            words.next().is_none().then_some(Self { kind, id, at })
        };
        parse().with_context(|| format!("malformed workload event {line:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both processes must agree on every message, including the
    /// two-argument repeat whose argument order is easy to swap.
    #[test]
    fn messages_round_trip() {
        for command in [
            Command::Commit(7),
            Command::Repeat {
                frame: 9,
                content: 3,
            },
            Command::Arm(u64::MAX),
            Command::Stop,
        ] {
            assert_eq!(command.to_string().parse::<Command>().unwrap(), command);
        }
        for kind in EventKind::ALL {
            let event = Event {
                kind,
                id: 42,
                at: Duration::new(1_234, 567_890_123),
            };
            assert_eq!(event.to_string().parse::<Event>().unwrap(), event);
        }
    }
}
