use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

use super::config::Transport;
use crate::{Result, fail};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Uploads {
    pub shared: usize,
    pub inline: usize,
}

#[derive(Debug)]
pub(crate) enum Event {
    Ready,
    Uploaded(Transport),
    Armed(u64),
    Committed { id: u64, at: Instant },
    Presented { id: u64, at: Instant },
    Input { id: u64, at: Instant },
    Released(u64),
    Error(String),
}

#[derive(Default)]
pub(crate) struct Record {
    pub ready: bool,
    pub uploads: Uploads,
    pub armed: Option<u64>,
    pub released: Option<u64>,
    pub commits: BTreeMap<u64, Instant>,
    pub frames: BTreeMap<u64, Instant>,
    pub inputs: BTreeMap<u64, Instant>,
}

impl Record {
    pub(crate) fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Ready => self.ready = true,
            Event::Uploaded(Transport::Shared) => self.uploads.shared += 1,
            Event::Uploaded(Transport::Inline) => self.uploads.inline += 1,
            Event::Armed(id) => self.armed = Some(id),
            Event::Released(id) => self.released = Some(id),
            Event::Committed { id, at } => {
                self.commits.entry(id).or_insert(at);
            }
            Event::Presented { id, at } => {
                if id & super::fixture::LOAD_BIT == 0 {
                    self.frames.entry(id).or_insert(at);
                }
            }
            Event::Input { id, at } => {
                self.inputs.entry(id).or_insert(at);
            }
            Event::Error(error) => return Err(fail(error)),
        }
        Ok(())
    }

    pub(crate) fn frame_latency(&self, id: u64) -> Result<Duration> {
        let start = self
            .commits
            .get(&id)
            .ok_or_else(|| fail(format!("missing commit {id}")))?;
        let end = self
            .frames
            .get(&id)
            .ok_or_else(|| fail(format!("missing presentation {id}")))?;
        end.checked_duration_since(*start)
            .ok_or_else(|| fail("presentation preceded its commit"))
    }

    pub(crate) fn prune(&mut self) {
        self.uploads = Uploads::default();
        self.commits.clear();
        self.frames.clear();
        self.inputs.clear();
    }
}

pub(crate) struct Row {
    pub scenario: String,
    pub phase: &'static str,
    pub run: usize,
    pub samples: Vec<Duration>,
    pub submitted: usize,
    pub late: usize,
    pub elapsed: Duration,
    pub uploads: Uploads,
}

impl Row {
    pub(crate) fn print(&mut self, csv: &mut Option<std::fs::File>) -> Result<()> {
        self.samples.sort_unstable();
        let count = self.samples.len();
        if count == 0 {
            return Err(fail(format!(
                "{} produced no matched samples",
                self.scenario
            )));
        }
        let at = |percent: usize| self.samples[(count * percent).div_ceil(100).saturating_sub(1)];
        let rate = f64::from(u32::try_from(count)?) / self.elapsed.as_secs_f64();
        let skipped = self.submitted.saturating_sub(count);
        let milliseconds = |duration: Duration| duration.as_secs_f64() * 1000.0;
        writeln!(
            std::io::stdout().lock(),
            "{:<29} {:<10} {:>3} {:>5}/{:<5} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>8.1} {:>5} {:>5} {:>5}/{:<5}",
            self.scenario,
            self.phase,
            self.run,
            count,
            self.submitted,
            milliseconds(at(50)),
            milliseconds(at(95)),
            milliseconds(at(99)),
            milliseconds(self.samples[count - 1]),
            rate,
            skipped,
            self.late,
            self.uploads.shared,
            self.uploads.inline,
        )?;
        if let Some(csv) = csv {
            writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                self.scenario,
                self.phase,
                self.run,
                count,
                self.submitted,
                at(50).as_nanos(),
                at(95).as_nanos(),
                at(99).as_nanos(),
                self.samples[count - 1].as_nanos(),
                self.elapsed.as_nanos(),
                rate,
                skipped,
                self.late,
                self.uploads.shared,
                self.uploads.inline,
            )?;
            csv.flush()?;
        }
        Ok(())
    }
}
