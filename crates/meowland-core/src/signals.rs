//! Signal subscriptions dispatched by the caller's calloop event loop.

use std::io;

use calloop::{
    LoopHandle, RegistrationToken,
    signals::{Signal as UnixSignal, Signals},
};
use futures::{StreamExt, channel::mpsc};

/// Unix signal requesting shutdown of the current session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationSignal {
    /// Interactive interrupt (`SIGINT`).
    Interrupt,
    /// Service termination (`SIGTERM`).
    Terminate,
    /// Controlling terminal disconnected (`SIGHUP`).
    Hangup,
}

/// One bounded signal subscription, retaining pending events on cancellation.
#[derive(Debug)]
pub struct Signal {
    receiver: mpsc::Receiver<UnixSignal>,
    handle: LoopHandle<'static, ()>,
    token: RegistrationToken,
}

impl Signal {
    /// Register a bounded, coalescing subscription on the terminal owner loop.
    ///
    /// # Errors
    /// Returns an error if creating or registering the signal source fails.
    pub fn new(handle: &LoopHandle<'static, ()>, signals: &[UnixSignal]) -> io::Result<Self> {
        let (mut sender, receiver) = mpsc::channel(signals.len());
        let token = handle
            .insert_source(Signals::new(signals)?, move |event, (), ()| {
                let _ = sender.try_send(event.signal());
            })
            .map_err(|error| io::Error::from(error.error))?;
        Ok(Self {
            receiver,
            handle: handle.clone(),
            token,
        })
    }

    /// Receive the next pending signal.
    #[expect(
        clippy::future_not_send,
        reason = "signal subscriptions belong to the owner thread"
    )]
    pub async fn recv(&mut self) -> Option<UnixSignal> {
        self.receiver.next().await
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        self.handle.remove(self.token);
    }
}

/// Termination subscriptions owned by one terminal session.
#[derive(Debug)]
pub struct Termination(Signal);

impl Termination {
    /// Subscribe before starting terminal session work.
    ///
    /// # Errors
    /// Returns an error if the signal source cannot be installed.
    pub fn new(handle: &LoopHandle<'static, ()>) -> io::Result<Self> {
        Signal::new(
            handle,
            &[UnixSignal::SIGINT, UnixSignal::SIGTERM, UnixSignal::SIGHUP],
        )
        .map(Self)
    }

    /// Wait for termination or hangup without losing events on cancellation.
    #[expect(
        clippy::future_not_send,
        reason = "signal subscriptions belong to the owner thread"
    )]
    pub async fn recv(&mut self) -> TerminationSignal {
        match self.0.recv().await {
            Some(UnixSignal::SIGINT) => TerminationSignal::Interrupt,
            Some(UnixSignal::SIGHUP) => TerminationSignal::Hangup,
            _ => TerminationSignal::Terminate,
        }
    }
}
