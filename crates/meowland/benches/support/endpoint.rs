//! PTY terminal emulation for capability replies and graphics consumption.

use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use pty_process::blocking::{self, Pts};
use rustix::{
    event::{PollFd, PollFlags, poll},
    termios::{self, OptionalActions},
};
use vtparse::{CollectingVTActor, CsiParam, VTAction, VTParser};

use super::{
    config::{Size, Transport},
    graphics::Graphics,
    record::Event,
};
use crate::{Result, fail};

pub(crate) struct Terminal {
    writer: Arc<Mutex<File>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Decoder {
    graphics: Graphics,
    size: Size,
    events: mpsc::Sender<Event>,
    writer: Arc<Mutex<File>>,
}

impl Decoder {
    fn reply(&self, bytes: &[u8]) -> Result<()> {
        self.writer
            .lock()
            .expect("PTY writer lock poisoned")
            .write_all(bytes)?;
        Ok(())
    }

    fn action(&mut self, action: VTAction) -> Result<()> {
        match action {
            VTAction::ApcDispatch(body) if body.starts_with(b"G") => {
                if let Some(reply) = self.graphics.command(&body[1..])? {
                    self.reply(&reply)?;
                }
                if let Some(transport) = self.graphics.uploaded.take() {
                    self.events.send(Event::Uploaded(transport))?;
                }
            }
            VTAction::CsiDispatch { params, byte, .. } => self.csi(&params, byte)?,
            VTAction::OscDispatch(fields)
                if fields.first().is_some_and(|field| field == b"52")
                    && fields.last().is_some_and(|field| field == b"?") =>
            {
                self.reply(b"\x1b]52;c;\x1b\\")?;
            }
            _ => {}
        }
        Ok(())
    }

    fn csi(&mut self, params: &[CsiParam], byte: u8) -> Result<()> {
        let numbers: Vec<usize> = params
            .iter()
            .filter_map(|param| match param {
                CsiParam::Integer(value) => usize::try_from(*value).ok(),
                CsiParam::P(_) => None,
            })
            .collect();
        let private = params.first() == Some(&CsiParam::P(b'?'));
        match byte {
            b't' if numbers == [16] => self.reply(b"\x1b[6;20;10t")?,
            b't' if numbers == [14] => {
                self.reply(format!("\x1b[4;{};{}t", self.size.height, self.size.width).as_bytes())?;
            }
            b'c' => self.reply(b"\x1b[?1;2c")?,
            b'u' if private => self.reply(b"\x1b[?31u")?,
            b'p' if private && numbers == [1016] => self.reply(b"\x1b[?1016;2$y")?,
            b'p' if private && numbers == [5522] => self.reply(b"\x1b[?5522;2$y")?,
            b'H' => {
                self.graphics.cursor = (
                    numbers.get(1).copied().unwrap_or(1).saturating_sub(1) * 10,
                    numbers.first().copied().unwrap_or(1).saturating_sub(1) * 20,
                );
            }
            b'l' if private && numbers == [2026] => {
                if let Some(id) = self.graphics.presented()? {
                    self.events.send(Event::Presented {
                        id,
                        at: Instant::now(),
                    })?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Terminal {
    pub(crate) fn start(
        size: Size,
        transport: Transport,
        delay: Duration,
        events: mpsc::Sender<Event>,
    ) -> Result<(Self, Pts)> {
        let (master, slave) = blocking::open()?;
        let flags = rustix::fs::fcntl_getfl(&master)?;
        rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK)?;
        let mut attributes = termios::tcgetattr(&slave)?;
        attributes.make_raw();
        termios::tcsetattr(&slave, OptionalActions::Now, &attributes)?;
        master.resize(pty_process::Size::new_with_pixel(
            u16::try_from(size.height / 20)?,
            u16::try_from(size.width / 10)?,
            u16::try_from(size.width)?,
            u16::try_from(size.height)?,
        ))?;
        let master = File::from(OwnedFd::from(master));
        let writer = Arc::new(Mutex::new(master.try_clone()?));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let mut decoder = Decoder {
            graphics: Graphics::new(size, transport),
            size,
            events: events.clone(),
            writer: Arc::clone(&writer),
        };
        let thread = thread::Builder::new()
            .name("bench-terminal".into())
            .spawn(move || {
                if let Err(error) = consume(master, &stopped, delay, &mut decoder)
                    && !stopped.load(Ordering::Relaxed)
                {
                    let _ = events.send(Event::Error(format!("terminal endpoint: {error}")));
                }
            })?;
        Ok((
            Self {
                writer,
                stop,
                thread: Some(thread),
            },
            slave,
        ))
    }

    pub(crate) fn inject(&self, bytes: &[u8]) -> Result<Instant> {
        let started = Instant::now();
        self.writer
            .lock()
            .expect("PTY writer lock poisoned")
            .write_all(bytes)?;
        Ok(started)
    }
}

fn consume(
    mut master: File,
    stop: &AtomicBool,
    delay: Duration,
    decoder: &mut Decoder,
) -> Result<()> {
    let mut parser = VTParser::new();
    let mut bytes = [0; 16 * 1024];
    let timeout = Duration::from_millis(50).try_into()?;
    while !stop.load(Ordering::Relaxed) {
        let ready = {
            let mut descriptors = [PollFd::new(&master, PollFlags::IN)];
            poll(&mut descriptors, Some(&timeout))?;
            descriptors[0].revents()
        };
        if ready.is_empty() {
            continue;
        }
        if !delay.is_zero() {
            thread::park_timeout(delay);
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let count = match master.read(&mut bytes) {
            Ok(0) => return Err(fail("pane PTY closed")),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.into()),
        };
        let mut actor = CollectingVTActor::default();
        parser.parse(&bytes[..count], &mut actor);
        for action in actor {
            decoder.action(action)?;
        }
    }
    Ok(())
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
