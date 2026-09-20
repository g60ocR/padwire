//! SIGINT/SIGTERM into an `AtomicBool`.
//!
//! The handler does nothing but store a flag, which is one of the few things
//! that is genuinely async-signal-safe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install the handler. Also ignores `SIGPIPE`, so a client vanishing
/// mid-write surfaces as `EPIPE` on the socket instead of killing the daemon.
pub fn install() {
    let h = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn shutting_down() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// A flag that follows the process-wide shutdown state, for threads that also
/// have their own reasons to stop.
pub fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn installing_twice_is_harmless() {
        super::install();
        super::install();
        assert!(!super::shutting_down());
    }
}
