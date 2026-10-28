//! Operation-owned readiness and deadline registrations.

use calloop::RegistrationToken;

use super::CompositorHandle;

pub(crate) struct Sources {
    handle: CompositorHandle,
    pub io: Option<RegistrationToken>,
    pub timer: Option<RegistrationToken>,
}

impl Sources {
    pub(crate) fn new(handle: &CompositorHandle) -> Self {
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
