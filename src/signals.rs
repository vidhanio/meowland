//! Shared signal handling for the server and terminal pane.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use signal_hook::SigId;

/// Termination handlers owned by one server or pane invocation.
#[derive(Debug)]
pub struct TerminationFlag {
    flag: Arc<AtomicBool>,
    handlers: [Option<SigId>; 3],
}

impl TerminationFlag {
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Drop for TerminationFlag {
    fn drop(&mut self) {
        for handler in self.handlers.iter().flatten() {
            signal_hook::low_level::unregister(*handler);
        }
    }
}

/// Register termination handlers until the returned guard is dropped.
///
/// # Errors
/// Returns an error when a signal handler cannot be registered.
pub fn termination_flag() -> std::io::Result<TerminationFlag> {
    let mut guard = TerminationFlag {
        flag: Arc::new(AtomicBool::new(false)),
        handlers: [None; 3],
    };
    for (slot, signal) in guard.handlers.iter_mut().zip([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ]) {
        *slot = Some(signal_hook::flag::register(
            signal,
            Arc::clone(&guard.flag),
        )?);
    }
    Ok(guard)
}
