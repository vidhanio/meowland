//! Non-overlapping workload definitions and identity-matched measurement
//! boundaries.

use std::{
    thread,
    time::{Duration, Instant},
};

use super::{
    app::{Command, Input},
    config::{Options, Size, Transport},
    fixture::Pattern,
    process::Session,
    record::Row,
};
use crate::{Result, fail};

const PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 120);
const RELEASE_BIT: u64 = 1 << 62;

#[derive(Clone, Copy)]
enum Interaction {
    Key,
    Pointer,
}
#[derive(Clone, Copy)]
enum Load {
    Idle,
    Rendering,
}
#[derive(Clone, Copy)]
enum Kind {
    Latency(Pattern),
    Throughput,
    Input(Interaction, Load),
}

pub(crate) struct Scenario {
    pub name: &'static str,
    size: Size,
    kind: Kind,
}

pub(crate) fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "frame/full/1080p",
            size: Size::HD,
            kind: Kind::Latency(Pattern::Noise),
        },
        Scenario {
            name: "frame/full/4k",
            size: Size::UHD,
            kind: Kind::Latency(Pattern::Noise),
        },
        Scenario {
            name: "frame/damage/1080p",
            size: Size::HD,
            kind: Kind::Latency(Pattern::Damage),
        },
        Scenario {
            name: "frame/throughput/4k",
            size: Size::UHD,
            kind: Kind::Throughput,
        },
        Scenario {
            name: "input/key/idle",
            size: Size::HD,
            kind: Kind::Input(Interaction::Key, Load::Idle),
        },
        Scenario {
            name: "input/key/render-load",
            size: Size::UHD,
            kind: Kind::Input(Interaction::Key, Load::Rendering),
        },
        Scenario {
            name: "input/pointer/render-load",
            size: Size::UHD,
            kind: Kind::Input(Interaction::Pointer, Load::Rendering),
        },
    ]
}

fn frame(session: &mut Session, id: u64) -> Result<Duration> {
    session.command(Command::Commit(id))?;
    session.wait(&format!("consuming frame {id}"), |record| {
        record.frames.contains_key(&id) && record.commits.contains_key(&id)
    })?;
    session.record.frame_latency(id)
}

fn row(
    scenario: &Scenario,
    phase: &'static str,
    run: usize,
    samples: Vec<Duration>,
    options: &Options,
    elapsed: Duration,
) -> Row {
    Row {
        scenario: scenario.name.into(),
        phase,
        run,
        samples,
        submitted: options.samples,
        late: 0,
        elapsed,
        uploads: super::record::Uploads::default(),
    }
}

fn latency(
    session: &mut Session,
    scenario: &Scenario,
    options: &Options,
    run: usize,
) -> Result<Vec<Row>> {
    for index in 1..=options.warmup {
        frame(session, u64::try_from(index)?)?;
    }
    session.record.prune();
    let started = Instant::now();
    let mut samples = Vec::with_capacity(options.samples);
    for index in 1..=options.samples {
        samples.push(frame(session, u64::try_from(options.warmup + index)?)?);
    }
    Ok(vec![row(
        scenario,
        "consume",
        run,
        samples,
        options,
        started.elapsed(),
    )])
}

fn throughput(
    session: &mut Session,
    scenario: &Scenario,
    options: &Options,
    run: usize,
) -> Result<Vec<Row>> {
    for index in 1..=options.warmup {
        frame(session, u64::try_from(index)?)?;
    }
    session.record.prune();
    let epoch = Instant::now();
    let mut late = 0;
    for index in 1..=options.samples {
        let scheduled = epoch + PERIOD * u32::try_from(index)?;
        thread::sleep(scheduled.saturating_duration_since(Instant::now()));
        session.command(Command::Commit(u64::try_from(options.warmup + index)?))?;
        if Instant::now().saturating_duration_since(scheduled) >= PERIOD {
            late += 1;
        }
        session.drain()?;
    }
    let last = u64::try_from(options.warmup + options.samples)?;
    session.wait("draining the final offered frame", |record| {
        record.frames.contains_key(&last) && record.commits.contains_key(&last)
    })?;
    session.drain()?;
    let elapsed = session.record.frames[&last].duration_since(epoch);
    let first = u64::try_from(options.warmup + 1)?;
    let samples = session
        .record
        .frames
        .range(first..=last)
        .map(|(id, _)| session.record.frame_latency(*id))
        .collect::<Result<Vec<_>>>()?;
    let mut result = row(scenario, "consume", run, samples, options, elapsed);
    result.late = late;
    Ok(vec![result])
}

fn interaction(
    session: &mut Session,
    id: u64,
    mode: Interaction,
    index: usize,
) -> Result<(Duration, Duration)> {
    let (input, bytes, key) = match mode {
        Interaction::Key => {
            let (ascii, code) = [(97, 30), (115, 31), (100, 32), (102, 33)][index % 4];
            (
                Input::Key(code),
                format!("\x1b[{ascii};1:1u"),
                Some((ascii, code)),
            )
        }
        Interaction::Pointer => {
            let x = u32::try_from(100 + index % 500)?;
            let y = u32::try_from(200 + index % 300)?;
            (Input::Pointer(x, y), format!("\x1b[<35;{x};{y}M"), None)
        }
    };
    session.command(Command::Arm { id, input })?;
    session.wait("arming input identity", |record| record.armed == Some(id))?;
    let started = session.inject(bytes.as_bytes())?;
    session.wait(
        &format!("delivery and visible response for input {id}"),
        |record| record.inputs.contains_key(&id) && record.frames.contains_key(&id),
    )?;
    let delivery = session.record.inputs[&id]
        .checked_duration_since(started)
        .ok_or_else(|| fail("stale input satisfied delivery"))?;
    let response = session.record.frames[&id]
        .checked_duration_since(started)
        .ok_or_else(|| fail("stale frame satisfied input response"))?;
    if let Some((ascii, code)) = key {
        let release = id | RELEASE_BIT;
        session.command(Command::Arm {
            id: release,
            input: Input::Release(code),
        })?;
        session.wait("arming key release", |record| record.armed == Some(release))?;
        session.inject(format!("\x1b[{ascii};1:3u").as_bytes())?;
        session.wait("matching key release", |record| {
            record.released == Some(release)
        })?;
    }
    Ok((delivery, response))
}

fn input(
    session: &mut Session,
    scenario: &Scenario,
    options: &Options,
    run: usize,
    mode: Interaction,
    load: Load,
) -> Result<Vec<Row>> {
    session.command(match load {
        Load::Idle => Command::StopLoad,
        Load::Rendering => Command::StartLoad,
    })?;
    for index in 1..=options.warmup {
        interaction(session, u64::try_from(index)?, mode, index)?;
    }
    session.record.prune();
    let started = Instant::now();
    let mut deliveries = Vec::with_capacity(options.samples);
    let mut responses = Vec::with_capacity(options.samples);
    for index in 1..=options.samples {
        let (delivery, response) = interaction(
            session,
            u64::try_from(options.warmup + index)?,
            mode,
            options.warmup + index,
        )?;
        deliveries.push(delivery);
        responses.push(response);
    }
    let elapsed = started.elapsed();
    session.command(Command::StopLoad)?;
    Ok(vec![
        row(scenario, "delivery", run, deliveries, options, elapsed),
        row(scenario, "response", run, responses, options, elapsed),
    ])
}

pub(crate) fn run(scenario: &Scenario, options: &Options, run: usize) -> Result<Vec<Row>> {
    let pattern = match scenario.kind {
        Kind::Latency(pattern) => pattern,
        Kind::Input(_, Load::Idle) => Pattern::Damage,
        _ => Pattern::Noise,
    };
    let mut session = Session::new(options, scenario.size, pattern)?;
    let mut rows = match scenario.kind {
        Kind::Latency(_) => latency(&mut session, scenario, options, run),
        Kind::Throughput => throughput(&mut session, scenario, options, run),
        Kind::Input(mode, load) => input(&mut session, scenario, options, run, mode, load),
    }?;
    session.drain()?;
    let uploads = session.record.uploads;
    if matches!(pattern, Pattern::Noise)
        && options.transport == Transport::Shared
        && uploads.shared == 0
    {
        return Err(fail(
            "shared transport requested, but no measured SHM image was consumed",
        ));
    }
    if options.transport == Transport::Inline && uploads.shared != 0 {
        return Err(fail(
            "inline transport requested, but SHM images were consumed",
        ));
    }
    for row in &mut rows {
        row.uploads = uploads;
    }
    Ok(rows)
}
