//! Which devices are currently exported.
//!
//! USB/IP allows exactly one importer per device — `vhci-hcd` on two hosts
//! would be two kernels driving the same endpoints. Enforcing it here turns a
//! confusing double-attach into a clean `ST_NA` on the second request.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Registry {
    busy: Mutex<HashSet<String>>,
}

impl Registry {
    pub fn new() -> Arc<Registry> {
        Arc::new(Registry::default())
    }

    /// Take the device, or `None` if someone already has it.
    pub fn acquire(self: &Arc<Self>, busid: &str) -> Option<Lease> {
        let mut busy = self.busy.lock().unwrap();
        if !busy.insert(busid.to_owned()) {
            return None;
        }
        Some(Lease {
            registry: Arc::clone(self),
            busid: busid.to_owned(),
        })
    }

    #[cfg(test)]
    pub fn in_use(&self, busid: &str) -> bool {
        self.busy.lock().unwrap().contains(busid)
    }
}

/// Releases the device when the session ends, however it ends.
pub struct Lease {
    registry: Arc<Registry>,
    busid: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.registry.busy.lock().unwrap().remove(&self.busid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_can_only_be_leased_once() {
        let r = Registry::new();
        let a = r.acquire("1-2").expect("first lease");
        assert!(r.acquire("1-2").is_none(), "second import must be refused");
        assert!(
            r.acquire("1-3").is_some(),
            "a different device is unaffected"
        );
        assert!(r.in_use("1-2"));
        drop(a);
        assert!(!r.in_use("1-2"));
        assert!(
            r.acquire("1-2").is_some(),
            "released devices can be re-imported"
        );
    }

    #[test]
    fn a_panicking_session_still_releases_its_device() {
        let r = Registry::new();
        let r2 = Arc::clone(&r);
        let h = std::thread::spawn(move || {
            let _lease = r2.acquire("1-4").unwrap();
            panic!("session died");
        });
        assert!(h.join().is_err());
        assert!(
            !r.in_use("1-4"),
            "the lease must have been dropped by unwinding"
        );
    }
}
