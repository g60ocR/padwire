//! Keeping an interrupt IN URB queued on the device at all times.
//!
//! Without this, the exporter only submits a URB when a `USBIP_CMD_SUBMIT`
//! arrives, so for a whole round trip *no URB is queued on the controller at
//! all* and every report it generates in that window is dropped by the
//! exporting kernel. A button press therefore waits, on average, half a round
//! trip for the next submit to show up and another half to travel back.
//!
//! With a URB always queued, the press is captured the moment it happens and
//! leaves on the next submit. The rate is still capped at one report per round
//! trip — `usbhid` keeps exactly one URB in flight and only resubmits on
//! completion, which no amount of work on this side can change — but each
//! report is about half as old by the time it lands. Measured against a tablet
//! two cities away: ~28 ms round trip, 36 reports/second either way, but the
//! report handed over is ~14 ms old instead of ~28 ms.
//!
//! This is a deliberate change of meaning, which is why it is behind a flag.
//! Faithful forwarding submits what the client asked for, when it asked.
//! Prefetching answers with input that was captured *before* the request
//! arrived. For a HID gamepad that is exactly right. For a device where each
//! transfer has to be requested to happen, it would be wrong.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use usb_backend::{SubmitError, UrbRequest, UsbBackend};
use usbfwd_common::{debug, warn};
use usbip_proto::pdu::Direction;

/// Internal URBs get seqnums from here up, so they cannot collide with the
/// client's. `vhci` numbers its own from 1 and increments, and a session that
/// reached 2^31 submits would have been running for years at any plausible
/// rate.
const INTERNAL_BASE: u32 = 0x8000_0000;

/// How many captured reports to hold per endpoint before dropping.
///
/// Not 1: two reports can easily arrive within one round trip — a quick tap is
/// a press and a release — and keeping only the newest would swallow the press
/// entirely, turning a real input into nothing. Not large either: anything
/// still queued is latency, so on overflow the *oldest* goes, which bounds how
/// far behind the controller's true state a reply can be while still letting
/// brief edges through.
const DEPTH: usize = 4;

/// Give up on an endpoint after this many consecutive failed prefetches and
/// fall back to pass-through, rather than spinning on a device that is
/// refusing.
const MAX_CONSECUTIVE_ERRORS: u32 = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Take {
    /// A captured report is ready; answer the client with it now.
    Ready(Vec<u8>),
    /// Nothing captured yet. The client's seqnum is registered and will be
    /// answered when the next report lands.
    Waiting,
    /// Not a prefetched endpoint. Submit it the ordinary way.
    PassThrough,
}

struct Endpoint {
    /// Endpoint number without the direction bit, as `submit` wants it.
    number: u8,
    max_packet: usize,
    /// Seqnum of the internal URB in flight, if any.
    in_flight: Option<u32>,
    captured: VecDeque<Vec<u8>>,
    /// A client submit that arrived with nothing captured.
    waiting: Option<Waiter>,
    errors: u32,
    /// Set once this endpoint has given up; it then behaves as pass-through.
    disabled: bool,
}

struct Waiter {
    seqnum: u32,
    max_len: usize,
}

struct Inner {
    eps: HashMap<u8, Endpoint>,
    next_seqnum: u32,
}

pub struct Prefetch {
    inner: Option<Mutex<Inner>>,
}

/// What the reaper should do with an internal completion.
pub struct Delivery {
    /// A client was waiting: answer this seqnum with this data and status.
    pub reply: Option<(u32, i32, Vec<u8>)>,
}

impl Prefetch {
    /// Disabled: every endpoint is pass-through and no internal URBs exist.
    pub fn disabled() -> Prefetch {
        Prefetch { inner: None }
    }

    pub fn new(dev: &dyn UsbBackend, enabled: bool) -> Prefetch {
        if !enabled {
            return Prefetch::disabled();
        }
        let eps: HashMap<u8, Endpoint> = dev
            .interrupt_in_endpoints()
            .into_iter()
            .map(|(address, max_packet)| {
                (
                    address,
                    Endpoint {
                        number: address & 0x0f,
                        max_packet: max_packet as usize,
                        in_flight: None,
                        captured: VecDeque::new(),
                        waiting: None,
                        errors: 0,
                        disabled: false,
                    },
                )
            })
            .collect();
        if eps.is_empty() {
            debug!("prefetch: no interrupt IN endpoints; staying disabled");
            return Prefetch::disabled();
        }
        debug!(
            "prefetch: enabled on {} interrupt IN endpoint(s)",
            eps.len()
        );
        Prefetch {
            inner: Some(Mutex::new(Inner {
                eps,
                next_seqnum: INTERNAL_BASE,
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// True for seqnums this module issued, so the reaper can tell an internal
    /// completion from one the client is waiting on.
    pub fn owns(seqnum: u32) -> bool {
        seqnum >= INTERNAL_BASE
    }

    /// Submit an internal URB on every endpoint that has none in flight.
    pub fn arm(&self, dev: &dyn UsbBackend) {
        let Some(lock) = &self.inner else { return };
        let mut inner = lock.lock().unwrap();
        let addrs: Vec<u8> = inner.eps.keys().copied().collect();
        for addr in addrs {
            inner.arm_one(addr, dev);
        }
    }

    /// A client submit arrived. Either hand back a captured report, register
    /// the client as waiting, or say this endpoint is not ours.
    ///
    /// Also re-arms the endpoint if nothing is in flight. Normally a completion
    /// re-arms it, but after a refused submit there is no completion to come,
    /// and a client left waiting then would wait forever.
    pub fn take(&self, dev: &dyn UsbBackend, address: u8, seqnum: u32, max_len: usize) -> Take {
        let Some(lock) = &self.inner else {
            return Take::PassThrough;
        };
        let mut inner = lock.lock().unwrap();
        let Some(ep) = inner.eps.get_mut(&address) else {
            return Take::PassThrough;
        };
        if ep.disabled {
            return Take::PassThrough;
        }
        let take = match ep.captured.pop_front() {
            Some(mut data) => {
                data.truncate(max_len);
                Take::Ready(data)
            }
            None => {
                // Only one client submit can be outstanding per endpoint,
                // because usbhid keeps exactly one URB in flight. If a second
                // arrives, the first is stale by definition; answering the
                // newer one is correct.
                ep.waiting = Some(Waiter { seqnum, max_len });
                Take::Waiting
            }
        };
        inner.arm_one(address, dev);
        take
    }

    /// An internal URB completed. Returns what the reaper owes the client, if
    /// anything, and re-arms the endpoint.
    pub fn deliver(
        &self,
        seqnum: u32,
        status: i32,
        data: Vec<u8>,
        dev: &dyn UsbBackend,
    ) -> Delivery {
        let Some(lock) = &self.inner else {
            return Delivery { reply: None };
        };
        let mut inner = lock.lock().unwrap();
        let Some(address) = inner
            .eps
            .iter()
            .find(|(_, e)| e.in_flight == Some(seqnum))
            .map(|(a, _)| *a)
        else {
            return Delivery { reply: None };
        };

        let mut reply = None;
        {
            let ep = inner.eps.get_mut(&address).expect("just found");
            ep.in_flight = None;

            if status != 0 {
                ep.errors += 1;
                // Hand the error on if someone is waiting: it is a real URB
                // outcome and the client's own submit would have seen it too.
                if let Some(w) = ep.waiting.take() {
                    reply = Some((w.seqnum, status, Vec::new()));
                }
                if ep.errors >= MAX_CONSECUTIVE_ERRORS {
                    warn!(
                        "prefetch: endpoint {address:#04x} failed {} times in a row \
                         (last status {status}); falling back to pass-through",
                        ep.errors
                    );
                    ep.disabled = true;
                    ep.captured.clear();
                }
            } else {
                ep.errors = 0;
                match ep.waiting.take() {
                    Some(w) => {
                        let mut d = data;
                        d.truncate(w.max_len);
                        reply = Some((w.seqnum, 0, d));
                    }
                    None => {
                        if ep.captured.len() == DEPTH {
                            ep.captured.pop_front();
                        }
                        ep.captured.push_back(data);
                    }
                }
            }
        }
        inner.arm_one(address, dev);
        Delivery { reply }
    }

    /// Drop a registered waiter, for `USBIP_CMD_UNLINK`. Returns true if this
    /// seqnum was one of ours to cancel.
    pub fn unlink(&self, seqnum: u32) -> bool {
        let Some(lock) = &self.inner else {
            return false;
        };
        let mut inner = lock.lock().unwrap();
        for ep in inner.eps.values_mut() {
            if ep.waiting.as_ref().is_some_and(|w| w.seqnum == seqnum) {
                ep.waiting = None;
                return true;
            }
        }
        false
    }

    /// Cancel the internal URBs. The device's own `cancel_pending` would catch
    /// them anyway; doing it here keeps the bookkeeping honest and stops a
    /// completion being re-armed during teardown.
    pub fn shutdown(&self, dev: &dyn UsbBackend) {
        let Some(lock) = &self.inner else { return };
        let mut inner = lock.lock().unwrap();
        for ep in inner.eps.values_mut() {
            ep.disabled = true;
            ep.waiting = None;
            ep.captured.clear();
            if let Some(s) = ep.in_flight.take() {
                let _ = dev.unlink(s);
            }
        }
    }
}

impl Inner {
    fn arm_one(&mut self, address: u8, dev: &dyn UsbBackend) {
        let seqnum = self.next_seqnum;
        let Some(ep) = self.eps.get_mut(&address) else {
            return;
        };
        // Armed even with the buffer full: a full buffer drops its oldest
        // report when the next one lands. Refusing here instead would leave
        // nothing in flight, and nothing would ever arm it again.
        if ep.disabled || ep.in_flight.is_some() {
            return;
        }
        let req = UrbRequest {
            seqnum,
            ep: ep.number,
            dir: Direction::In,
            transfer_flags: 0,
            setup: [0u8; 8],
            buffer_length: ep.max_packet,
            data: Vec::new(),
        };
        match dev.submit(req) {
            Ok(()) => {
                ep.in_flight = Some(seqnum);
                self.next_seqnum = self.next_seqnum.wrapping_add(1).max(INTERNAL_BASE);
            }
            Err(SubmitError::Urb(status)) => {
                ep.errors += 1;
                if ep.errors >= MAX_CONSECUTIVE_ERRORS {
                    warn!(
                        "prefetch: endpoint {address:#04x} refused {} submits in a row \
                         (last errno {}); falling back to pass-through",
                        ep.errors, -status
                    );
                    ep.disabled = true;
                }
            }
            Err(SubmitError::Handled(_)) => {}
            Err(SubmitError::Fatal(e)) => {
                warn!("prefetch: endpoint {address:#04x} is gone ({e}); disabling");
                ep.disabled = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_seqnums_cannot_collide_with_a_clients() {
        // vhci starts at 1 and counts up; everything it will realistically
        // issue is below the internal range.
        assert!(!Prefetch::owns(1));
        assert!(!Prefetch::owns(1_000_000));
        assert!(!Prefetch::owns(INTERNAL_BASE - 1));
        assert!(Prefetch::owns(INTERNAL_BASE));
        assert!(Prefetch::owns(u32::MAX));
    }

    #[test]
    fn disabled_prefetch_passes_everything_through() {
        let p = Prefetch::disabled();
        assert!(!p.is_enabled());
        assert_eq!(p.take(&Fake::default(), 0x81, 7, 64), Take::PassThrough);
        assert!(!p.unlink(7));
    }

    /// Records submits and never completes anything on its own; the tests
    /// complete URBs by calling `deliver`.
    #[derive(Default)]
    struct Fake {
        submitted: Mutex<Vec<u32>>,
        refuse: Mutex<bool>,
    }

    impl Fake {
        fn last(&self) -> u32 {
            *self
                .submitted
                .lock()
                .unwrap()
                .last()
                .expect("something submitted")
        }
    }

    impl UsbBackend for Fake {
        fn summary(&self) -> &usb_backend::DeviceSummary {
            unimplemented!("prefetch never asks")
        }
        fn claim_all(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn release_all(&self) {}
        fn submit(&self, req: UrbRequest) -> Result<(), SubmitError> {
            if *self.refuse.lock().unwrap() {
                return Err(SubmitError::Urb(-libc::EAGAIN));
            }
            self.submitted.lock().unwrap().push(req.seqnum);
            Ok(())
        }
        fn unlink(&self, _: u32) -> std::io::Result<bool> {
            Ok(true)
        }
        fn wait(&self, _: std::time::Duration) -> std::io::Result<usb_backend::Wake> {
            unimplemented!("prefetch never waits")
        }
        fn reap(&self) -> std::io::Result<Option<usb_backend::UrbCompletion>> {
            Ok(None)
        }
        fn interrupt_in_endpoints(&self) -> Vec<(u8, u16)> {
            vec![(0x81, 64)]
        }
        fn cancel_pending(&self) {}
    }

    fn in_flight(p: &Prefetch) -> Option<u32> {
        p.inner.as_ref().unwrap().lock().unwrap().eps[&0x81].in_flight
    }

    #[test]
    fn a_full_buffer_keeps_capturing_and_drops_the_oldest() {
        // A streaming controller easily produces more reports in one round
        // trip than the buffer holds. This used to stop arming at DEPTH, so
        // once the client drained the buffer its next submit waited forever.
        let dev = Fake::default();
        let p = Prefetch::new(&dev, true);
        p.arm(&dev);
        for i in 0..6u8 {
            p.deliver(dev.last(), 0, vec![i], &dev);
        }
        assert!(
            in_flight(&p).is_some(),
            "still capturing with the buffer full"
        );

        for want in 2..6u8 {
            assert_eq!(p.take(&dev, 0x81, want as u32, 64), Take::Ready(vec![want]));
        }
        assert_eq!(p.take(&dev, 0x81, 10, 64), Take::Waiting);
        let d = p.deliver(dev.last(), 0, vec![6], &dev);
        assert_eq!(d.reply, Some((10, 0, vec![6])));
    }

    #[test]
    fn a_client_submit_rearms_after_a_refused_one() {
        let dev = Fake::default();
        let p = Prefetch::new(&dev, true);
        *dev.refuse.lock().unwrap() = true;
        p.arm(&dev);
        assert_eq!(in_flight(&p), None);

        *dev.refuse.lock().unwrap() = false;
        assert_eq!(p.take(&dev, 0x81, 1, 64), Take::Waiting);
        assert!(
            in_flight(&p).is_some(),
            "the waiting client has a URB to answer it"
        );
        let d = p.deliver(dev.last(), 0, vec![9], &dev);
        assert_eq!(d.reply, Some((1, 0, vec![9])));
    }
}
