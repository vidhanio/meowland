//! Ordered input deferral while a terminal clipboard refresh is pending.

use std::time::Instant;

use meowland_protocol::{Input, PaneAction, PaneToServer, asynchronous::Outgoing};

#[derive(Default)]
pub(crate) struct InputGate {
    pending: Option<Deferred>,
}

struct Deferred {
    inputs: Vec<Input>,
    deadline: Instant,
}

impl InputGate {
    pub(crate) const fn deadline(&self) -> Option<Instant> {
        match &self.pending {
            Some(pending) => Some(pending.deadline),
            None => None,
        }
    }

    pub(crate) const fn blocked(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn expired(&self) -> bool {
        self.deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(crate) fn send(
        &mut self,
        input: Input,
        defer: Option<Instant>,
        tx: &mut Outgoing,
    ) -> std::io::Result<()> {
        if let Some(deadline) = self.deadline().or(defer) {
            let pending = self.pending.get_or_insert_with(|| Deferred {
                inputs: Vec::new(),
                deadline,
            });
            pending.inputs.push(input);
            if pending.inputs.len() >= 256 {
                tracing::warn!(
                    "Clipboard refresh deferred too much input; forwarding original events"
                );
                self.flush(tx)?;
            }
        } else {
            tx.send(&PaneToServer::Action(PaneAction::Input(input)))?;
        }
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        self.pending = None;
    }

    pub(crate) fn flush(&mut self, tx: &mut Outgoing) -> std::io::Result<()> {
        if let Some(pending) = self.pending.take() {
            for input in pending.inputs {
                tx.send(&PaneToServer::Action(PaneAction::Input(input)))?;
            }
        }
        Ok(())
    }
}
