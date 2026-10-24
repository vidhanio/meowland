//! Operation-owned readiness and deadline registrations.

use calloop::{LoopHandle, RegistrationToken};

use super::State;

pub struct Sources {
    handle: LoopHandle<'static, State>,
    pub io: Option<RegistrationToken>,
    pub timer: Option<RegistrationToken>,
}

impl Sources {
    pub fn new(handle: &LoopHandle<'static, State>) -> Self {
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
