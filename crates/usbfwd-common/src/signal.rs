//! SIGINT/SIGTERM into an `AtomicBool`.
//!
//! The handler does nothing but store a flag, which is one of the few things
//! that is genuinely async-signal-safe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static HANGUP: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

extern "C" fn hangup(_sig: libc::c_int) {
    HANGUP.store(true, Ordering::SeqCst);
}

/// Install the handler. Also ignores `SIGPIPE`, so a client vanishing
/// mid-write surfaces as `EPIPE` on the socket instead of killing the daemon.
pub fn install() {
    let h = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(
            libc::SIGHUP,
            hangup as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

/// Take the pending `SIGHUP`, clearing it.
///
/// The exporter uses it as the way out of a toggle it cannot undo on its own:
/// whatever the chord watcher is doing, a `SIGHUP` puts every withheld device
/// back on offer. That matters because the toggle deliberately takes the
/// controls away from the machine running this process, so there has to be a
/// lever that does not need those controls to reach.
pub fn take_hangup() -> bool {
    HANGUP.swap(false, Ordering::SeqCst)
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
