//! Operation-owned readiness and deadline registrations.

use calloop::{LoopHandle, RegistrationToken};

use super::State;

pub(super) struct Sources {
    handle: LoopHandle<'static, State>,
    pub(super) io: Option<RegistrationToken>,
    pub(super) timer: Option<RegistrationToken>,
}

impl Sources {
    pub(super) fn new(handle: &LoopHandle<'static, State>) -> Self {
        Self {
            handle: handle.clone(),
            io: None,
            timer: None,
        }
    }
}

impl Drop for Sources {
    fn drop(&mut self) {
        for token in [self.io.take(), self.timer.take()].into_iter().flatten() {
            self.handle.remove(token);
        }
    }
}
