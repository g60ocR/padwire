//! Taking a device off offer, and the way back.
//!
//! Forwarding a handheld's *own* controls is the one export that costs you the
//! machine you are holding: the exporter evicts the kernel's driver, so the
//! Deck's UI stops responding for as long as the session lasts. The toggle
//! makes that reversible from the controller itself.
//!
//! The shape of it matters more than the mechanism. "Off" is not a pause in
//! the URB pump — it is **the device no longer being on offer**:
//!
//! * the session ends, which is the already-load-bearing teardown path, so
//!   every interface is released and rebound exactly as it is on a detach;
//! * [`Gated`] drops the bus id from `OP_REP_DEVLIST` and refuses
//!   `OP_REQ_IMPORT` for it, so the importing daemon's reconnect loop sees a
//!   device that is simply not there and waits, which is behaviour it already
//!   has;
//! * [`watch`] reads the chord back off `hidraw` — the node the rebound driver
//!   just created — and puts the device back on offer, at which point the
//!   importer's next poll attaches it again.
//!
//! Nothing new is invented on the wire and nothing new is invented on the host.
//!
//! ## When the way back is blocked
//!
//! The watcher needs read access to `/dev/hidraw*` (see the udev rule in
//! `packaging/`). Without it the chord can suspend a device and never resume
//! it, so a failed watcher keeps the device **suspended** rather than quietly
//! restoring the forward: the person holding the Deck keeps their controls,
//! the log says exactly what to install, and `SIGHUP` — or restarting the
//! service — puts everything back on offer without needing the controller.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usb_backend::hidraw::{self, Tap};
use usb_backend::{enumerate, DeviceFilter};
use usbfwd_common::{debug, info, signal, warn};

use crate::chord::{self, Chord, Detector};
use crate::{DeviceSource, Stop};

/// How long the watcher blocks on `hidraw` before re-checking for shutdown.
const POLL: Duration = Duration::from_millis(250);

/// How long a rebinding driver gets to create its `hidraw` node before the
/// watcher starts complaining. Releasing seven interfaces and rebinding each
/// is not instant, and a warning during the normal handover would be noise.
const REBIND_GRACE: Duration = Duration::from_secs(3);

/// How often to repeat the complaint while a suspended device has no way back.
const NAG: Duration = Duration::from_secs(30);

/// Which devices are currently withheld from importers.
#[derive(Default)]
pub struct Toggle {
    suspended: Mutex<HashSet<String>>,
}

impl Toggle {
    pub fn new() -> Arc<Toggle> {
        Arc::new(Toggle::default())
    }

    pub fn is_suspended(&self, busid: &str) -> bool {
        self.suspended.lock().unwrap().contains(busid)
    }

    /// Returns false if it was already suspended.
    pub fn suspend(&self, busid: &str) -> bool {
        self.suspended.lock().unwrap().insert(busid.to_owned())
    }

    /// Returns false if it was not suspended.
    pub fn resume(&self, busid: &str) -> bool {
        self.suspended.lock().unwrap().remove(busid)
    }

    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = self.suspended.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }

    /// Put everything back on offer. The `SIGHUP` escape hatch.
    pub fn resume_all(&self) -> Vec<String> {
        let mut s = self.suspended.lock().unwrap();
        let was: Vec<String> = s.iter().cloned().collect();
        s.clear();
        was
    }
}

/// Everything a session needs to recognise the chord and act on it.
#[derive(Clone)]
pub struct Hotkey {
    pub chord: Chord,
    pub hold: Duration,
    pub toggle: Arc<Toggle>,
}

impl Hotkey {
    pub fn detector(&self) -> Detector {
        Detector::new(self.chord, self.hold)
    }
}

/// A [`DeviceSource`] that hides whatever the toggle has suspended.
pub struct Gated {
    inner: Arc<dyn DeviceSource>,
    toggle: Arc<Toggle>,
}

impl Gated {
    pub fn new(inner: Arc<dyn DeviceSource>, toggle: Arc<Toggle>) -> Gated {
        Gated { inner, toggle }
    }
}

impl DeviceSource for Gated {
    fn list(&self) -> io::Result<Vec<usb_backend::DeviceSummary>> {
        Ok(self
            .inner
            .list()?
            .into_iter()
            .filter(|d| !self.toggle.is_suspended(&d.busid))
            .collect())
    }

    fn open(&self, busid: &str) -> io::Result<Arc<usb_backend::UsbfsDevice>> {
        // `import` already checks the list, so reaching here means a race with
        // a chord that fired a moment ago. Refusing is the same answer the
        // list gives and costs nothing.
        if self.toggle.is_suspended(busid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{busid} is suspended by the toggle chord"),
            ));
        }
        self.inner.open(busid)
    }
}

/// Watch every suspended device for the chord that puts it back on offer.
///
/// One thread for all of them: a suspended device is idle by definition, so
/// there is nothing here worth a thread each.
pub fn watch(hotkey: Hotkey, stop: Stop) {
    let mut watched: HashMap<String, Watched> = HashMap::new();
    info!(
        "toggle: hold {} for {:?} to suspend a forward; the same chord resumes it",
        hotkey.chord, hotkey.hold
    );
    while !stop() {
        if signal::take_hangup() {
            for busid in hotkey.toggle.resume_all() {
                info!("toggle: SIGHUP, {busid} is on offer again");
            }
        }
        let suspended = hotkey.toggle.list();
        watched.retain(|busid, _| suspended.contains(busid));
        let mut polled = false;
        for busid in suspended {
            let w = watched
                .entry(busid.clone())
                .or_insert_with(|| Watched::new(busid.clone(), &hotkey));
            match w.step() {
                Step::Fired => {
                    hotkey.toggle.resume(&w.busid);
                    info!(
                        "toggle: {} held; {} is on offer again",
                        hotkey.chord, w.busid
                    );
                    polled = true;
                }
                Step::Polled => polled = true,
                Step::Idle => {}
            }
        }
        if !polled {
            thread::sleep(POLL);
        }
    }
}

enum Step {
    /// The chord fired: resume this device.
    Fired,
    /// A poll happened, so the loop has already paced itself.
    Polled,
    /// Nothing to poll — no usable `hidraw` node yet.
    Idle,
}

struct Watched {
    busid: String,
    hotkey: Hotkey,
    tap: Option<Tap>,
    detector: Detector,
    suspended_at: Instant,
    complained_at: Option<Instant>,
}

impl Watched {
    fn new(busid: String, hotkey: &Hotkey) -> Watched {
        Watched {
            busid,
            hotkey: hotkey.clone(),
            tap: None,
            detector: hotkey.detector(),
            suspended_at: Instant::now(),
            complained_at: None,
        }
    }

    fn step(&mut self) -> Step {
        if self.tap.is_none() {
            self.attach_tap();
        }
        let Some(tap) = self.tap.as_mut() else {
            self.complain();
            return Step::Idle;
        };
        match tap.read(POLL) {
            Ok(Some(reports)) => {
                let now = Instant::now();
                for r in reports {
                    if self.detector.feed(&r, now).fired {
                        return Step::Fired;
                    }
                }
                Step::Polled
            }
            // The node went away: the device was unplugged, or its driver
            // rebound underneath us. Rebuild on the next pass.
            Ok(None) | Err(_) => {
                debug!("toggle: {} lost its hidraw tap; reopening", self.busid);
                self.tap = None;
                Step::Idle
            }
        }
    }

    fn attach_tap(&mut self) {
        let nodes = match hidraw::nodes_for_busid(&self.busid) {
            Ok(n) if n.is_empty() => return,
            Ok(n) => n,
            Err(e) => {
                debug!("toggle: cannot look up hidraw for {}: {e}", self.busid);
                return;
            }
        };
        let (tap, errors) = Tap::open(&nodes);
        if tap.is_empty() {
            for (p, e) in errors {
                debug!("toggle: cannot open {}: {e}", p.display());
            }
            return;
        }
        // A fresh detector, so the chord that suspended this device — still
        // held while the driver was rebinding — has to be released before it
        // can resume it. Otherwise one long press would toggle twice.
        self.detector = self.hotkey.detector();
        debug!(
            "toggle: watching {} on {}",
            self.busid,
            tap.paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.complained_at = None;
        self.tap = Some(tap);
    }

    /// Say — once after the grace period, then occasionally — that this device
    /// has no way back. Staying quiet here would leave a handheld whose
    /// controls came back but whose forward never will, with nothing in the
    /// log to explain it.
    fn complain(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.suspended_at) < REBIND_GRACE {
            return;
        }
        if self
            .complained_at
            .is_some_and(|t| now.duration_since(t) < NAG)
        {
            return;
        }
        self.complained_at = Some(now);
        warn!(
            "toggle: {} is suspended but has no readable hidraw node, so the chord \
             cannot resume it. Install packaging/99-usbfwd.rules (it grants hidraw \
             access), or send SIGHUP to put it back on offer now.",
            self.busid
        );
    }
}

/// Say at startup whether the chord would be able to undo itself.
///
/// The failure this catches — a missing udev rule for `hidraw` — only shows
/// itself at the worst possible moment otherwise: the chord takes the forward
/// down and nothing brings it back. Right now, before any session, the devices
/// are unclaimed and their nodes are exactly the ones the watcher will want,
/// so the check is both cheap and honest.
pub fn preflight(filter: &DeviceFilter) {
    for d in enumerate::list(filter).unwrap_or_default() {
        let nodes = match hidraw::nodes_for_busid(&d.busid) {
            Ok(n) if n.is_empty() => continue, // no HID interface, or already claimed
            Ok(n) => n,
            Err(_) => continue,
        };
        let (tap, errors) = Tap::open(&nodes);
        if !tap.is_empty() {
            continue;
        }
        let why = errors
            .first()
            .map(|(_, e)| e.to_string())
            .unwrap_or_else(|| "no node could be opened".into());
        warn!(
            "toggle: none of {}'s {} hidraw node(s) can be read ({why}), so the chord \
             could suspend it and not resume it. Install packaging/99-usbfwd.rules.",
            d.busid,
            nodes.len()
        );
    }
}

/// `--chord-probe`: print the buttons the allowed devices are sending.
///
/// The one piece of this feature that cannot be settled by reading code — the
/// mapping from a physical button to a bit — takes about ten seconds to settle
/// with this. It only ever reads, so it is safe to run while Steam has the
/// controller.
pub fn probe(filter: &DeviceFilter) -> io::Result<()> {
    signal::install();
    let devices = enumerate::list(filter)?;
    if devices.is_empty() {
        println!("No device matches {filter}.");
        return Ok(());
    }

    let mut taps = Vec::new();
    for d in &devices {
        let nodes = hidraw::nodes_for_busid(&d.busid).unwrap_or_default();
        let (v, p) = d.vid_pid();
        if nodes.is_empty() {
            println!(
                "{} {v:04x}:{p:04x}: no hidraw node — it is claimed by usbfwd \
                 right now, or its driver is not bound",
                d.busid
            );
            continue;
        }
        let (tap, errors) = Tap::open(&nodes);
        for (path, e) in &errors {
            println!("{}: cannot open {}: {e}", d.busid, path.display());
        }
        if tap.is_empty() {
            println!(
                "{}: no readable hidraw node. Install packaging/99-usbfwd.rules.",
                d.busid
            );
            continue;
        }
        println!(
            "{} {v:04x}:{p:04x}: watching {}",
            d.busid,
            tap.paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        taps.push((d.busid.clone(), tap));
    }
    if taps.is_empty() {
        return Ok(());
    }

    println!("\nHold the buttons you want as the chord. Ctrl-C when done.\n");
    let mut last: HashMap<String, u64> = HashMap::new();
    while !signal::shutting_down() {
        for (busid, tap) in taps.iter_mut() {
            let Ok(Some(reports)) = tap.read(POLL) else {
                continue;
            };
            for r in reports {
                let Some(mask) = chord::buttons(&r) else {
                    // At -v, so a controller whose reports this does not
                    // understand can still be looked at rather than guessed
                    // about.
                    debug!(
                        "{busid}: {} bytes, not a state report: {}",
                        r.len(),
                        hex(&r)
                    );
                    continue;
                };
                if last.get(busid) == Some(&mask) {
                    continue;
                }
                last.insert(busid.clone(), mask);
                println!(
                    "{busid}  0x{mask:016x}  {}\n          --toggle-chord '{}'",
                    chord::describe(mask),
                    chord::describe(mask)
                );
            }
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use usb_backend::DeviceSummary;

    struct Fake(Vec<String>);

    impl DeviceSource for Fake {
        fn list(&self) -> io::Result<Vec<DeviceSummary>> {
            Ok(self
                .0
                .iter()
                .map(|busid| DeviceSummary {
                    busnum: 1,
                    devnum: 2,
                    busid: busid.clone(),
                    sys_path: String::new(),
                    node_path: String::new(),
                    speed: usb_backend::Speed::High,
                    device: Default::default(),
                    config_value: 1,
                    interfaces: Vec::new(),
                })
                .collect())
        }

        fn open(&self, _busid: &str) -> io::Result<Arc<usb_backend::UsbfsDevice>> {
            Err(io::Error::new(io::ErrorKind::Other, "not in this test"))
        }
    }

    #[test]
    fn suspending_takes_a_device_off_the_list_and_resuming_puts_it_back() {
        let toggle = Toggle::new();
        let gated = Gated::new(
            Arc::new(Fake(vec!["1-2".into(), "1-3".into()])),
            Arc::clone(&toggle),
        );
        let ids =
            |g: &Gated| -> Vec<String> { g.list().unwrap().into_iter().map(|d| d.busid).collect() };

        assert_eq!(ids(&gated), vec!["1-2", "1-3"]);
        assert!(toggle.suspend("1-2"));
        assert!(!toggle.suspend("1-2"), "suspending twice changes nothing");
        assert_eq!(
            ids(&gated),
            vec!["1-3"],
            "a suspended device is not offered"
        );
        assert_eq!(
            gated.open("1-2").err().map(|e| e.kind()),
            Some(io::ErrorKind::PermissionDenied),
            "and cannot be imported behind the list's back"
        );
        assert!(toggle.resume("1-2"));
        assert_eq!(ids(&gated), vec!["1-2", "1-3"]);
    }

    #[test]
    fn sighup_puts_everything_back() {
        let toggle = Toggle::new();
        toggle.suspend("1-2");
        toggle.suspend("1-3");
        assert_eq!(toggle.list(), vec!["1-2", "1-3"]);
        assert_eq!(toggle.resume_all().len(), 2);
        assert!(toggle.list().is_empty());
    }
}
