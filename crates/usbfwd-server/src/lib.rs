//! The server proper: binding, accepting, and turning one connection into an
//! export session. Kept out of `main.rs` so integration tests can drive the
//! real listener, and so the Android build can reuse all of it.
//!
//! The only thing that differs between a Steam Deck and an Android tablet is
//! where devices come from: sysfs enumeration on one, a single file descriptor
//! handed over by `UsbManager` on the other. [`DeviceSource`] is that seam, and
//! everything below it — the handshake, the allow-list check, the one-importer
//! rule, the URB pump — is shared.

pub mod registry;
pub mod session;

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usb_backend::usbfs::DEFAULT_MAX_TRANSFER;
use usb_backend::{enumerate, DeviceFilter, DeviceSummary, UsbBackend, UsbfsDevice};
use usbfwd_common::{error, info, mdns, netif, signal, warn};
use usbip_proto::io::{read_op_request, OpRequest};
use usbip_proto::{op, USBIP_PORT};

use registry::Registry;

/// A handful of importers is already more than the hardware can be shared
/// with; the cap only exists so a connection flood cannot spawn threads
/// without bound.
pub const MAX_SESSIONS: usize = 16;

/// How long a client gets to send its first request, so a connect-and-say-
/// nothing peer cannot pin a device slot.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to let sessions finish on shutdown before giving up on them.
///
/// This matters more than it looks: a session's teardown is what releases the
/// device's interfaces *and asks the kernel to rebind its drivers*. Exiting
/// out from under a live session leaves the controller with no driver at all
/// until it is replugged, because closing the usbfs descriptor releases the
/// claims but never reconnects anything.
/// Generous on purpose: teardown releases every interface and then retries the
/// rebind of each, which on a seven-interface device can legitimately take
/// well over a second.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(8);

/// Something the caller polls to find out whether to keep going. The Linux
/// daemon wires this to SIGTERM; the Android service wires it to its own
/// lifecycle. Shared rather than borrowed because session threads outlive the
/// accept loop's stack frame.
pub type Stop = Arc<dyn Fn() -> bool + Send + Sync>;

/// A stop predicate that never fires, for tests and one-shot callers.
pub fn never_stop() -> Stop {
    Arc::new(|| false)
}

/// The process-wide SIGINT/SIGTERM flag.
pub fn signal_stop() -> Stop {
    Arc::new(signal::shutting_down)
}

/// Where exportable devices come from.
pub trait DeviceSource: Send + Sync {
    /// Everything this source is willing to export, already filtered.
    fn list(&self) -> std::io::Result<Vec<DeviceSummary>>;
    /// Open one by bus id. Callers only ever pass a bus id that [`list`]
    /// returned, so implementations do not have to re-check the allow-list.
    ///
    /// [`list`]: DeviceSource::list
    fn open(&self, busid: &str) -> std::io::Result<Arc<UsbfsDevice>>;
}

/// Enumerate the local USB bus through sysfs and open `/dev/bus/usb` nodes.
pub struct SysfsDevices {
    pub filter: DeviceFilter,
    pub max_transfer: usize,
}

impl DeviceSource for SysfsDevices {
    fn list(&self) -> std::io::Result<Vec<DeviceSummary>> {
        enumerate::list(&self.filter)
    }

    fn open(&self, busid: &str) -> std::io::Result<Arc<UsbfsDevice>> {
        Ok(Arc::new(
            UsbfsDevice::open_busid(busid)?.with_max_transfer(self.max_transfer),
        ))
    }
}

/// Export exactly one already-open device. This is the Android case: the
/// descriptor arrives from `UsbDeviceConnection` and there is no sysfs to
/// enumerate.
pub struct SingleDevice {
    dev: Arc<UsbfsDevice>,
}

impl SingleDevice {
    pub fn new(dev: Arc<UsbfsDevice>) -> SingleDevice {
        SingleDevice { dev }
    }

    pub fn device(&self) -> &Arc<UsbfsDevice> {
        &self.dev
    }
}

impl DeviceSource for SingleDevice {
    fn list(&self) -> std::io::Result<Vec<DeviceSummary>> {
        Ok(vec![self.dev.summary().clone()])
    }

    fn open(&self, busid: &str) -> std::io::Result<Arc<UsbfsDevice>> {
        if busid == self.dev.summary().busid {
            Ok(Arc::clone(&self.dev))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("this exporter only offers {}", self.dev.summary().busid),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// Every address that looks like it belongs to the tailnet.
    Tailscale,
    /// Wildcard on both families.
    Any,
    Addr(IpAddr),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub binds: Vec<Bind>,
    pub port: u16,
    pub filter: DeviceFilter,
    pub max_transfer: usize,
    pub mdns: bool,
    pub name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            binds: vec![Bind::Tailscale],
            port: USBIP_PORT,
            filter: DeviceFilter::default(),
            max_transfer: DEFAULT_MAX_TRANSFER,
            mdns: false,
            name: None,
        }
    }
}

pub fn run(args: Config) -> std::io::Result<()> {
    if args.filter.is_empty() {
        warn!("the allow-list is empty, so no device can be exported");
    }

    signal::install();

    let listeners = bind_when_ready(&args)?;

    let source: Arc<dyn DeviceSource> = Arc::new(SysfsDevices {
        filter: args.filter.clone(),
        max_transfer: args.max_transfer,
    });

    match source.list() {
        Ok(d) if d.is_empty() => info!(
            "no device currently matches {} — plug one in, it will be picked up on the next request",
            args.filter
        ),
        Ok(d) => {
            for s in &d {
                let (v, p) = s.vid_pid();
                info!(
                    "exportable: {} {v:04x}:{p:04x} ({} interfaces)",
                    s.busid,
                    s.interfaces.len()
                );
            }
        }
        Err(e) => warn!("cannot enumerate devices: {e}"),
    }

    let _mdns = if args.mdns { start_mdns(&args) } else { None };

    accept_loop(listeners, source, &Registry::new(), signal_stop());
    info!("shutting down");
    Ok(())
}

/// Bind every address the configuration asks for. Binding some but not all is
/// not fatal — `::` frequently also covers `0.0.0.0` — but binding none is.
pub fn bind_all(args: &Config) -> std::io::Result<Vec<TcpListener>> {
    let mut listeners = Vec::new();
    for addr in resolve_binds(&args.binds)? {
        let sa = SocketAddr::new(addr, args.port);
        match TcpListener::bind(sa) {
            Ok(l) => {
                info!("listening on {sa}");
                listeners.push(l);
            }
            Err(e) => warn!("cannot bind {sa}: {e}"),
        }
    }
    if listeners.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no address could be bound",
        ));
    }
    Ok(listeners)
}

/// Bind, waiting for the tailnet to come up rather than failing.
///
/// On a handheld, `tailscaled` routinely finishes after this service starts.
/// Exiting and letting systemd restart would work, but it turns a normal boot
/// into a crash loop in the journal and loses the distinction between "not yet"
/// and "misconfigured".
fn bind_when_ready(args: &Config) -> std::io::Result<Vec<TcpListener>> {
    let waits_for_tailscale = args.binds.contains(&Bind::Tailscale);
    let mut announced = false;
    loop {
        match bind_all(args) {
            Ok(l) => return Ok(l),
            Err(e) if waits_for_tailscale && e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                if signal::shutting_down() {
                    return Err(e);
                }
                if !announced {
                    info!("waiting for a Tailscale address before listening");
                    announced = true;
                }
                thread::sleep(Duration::from_secs(5));
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn accept_loop(
    listeners: Vec<TcpListener>,
    source: Arc<dyn DeviceSource>,
    registry: &Arc<Registry>,
    stop: Stop,
) {
    // poll() over every listener keeps this to one thread and lets the loop
    // notice a shutdown request within a quarter second.
    let mut pfds: Vec<libc::pollfd> = listeners
        .iter()
        .map(|l| libc::pollfd {
            fd: l.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let sessions = Arc::new(AtomicUsize::new(0));
    // Live connections, so shutdown can unblock a reader that is sitting in
    // read() rather than waiting out the drain timeout for nothing.
    let live: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
    let next_id = AtomicU64::new(0);

    while !stop() {
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 250) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            error!("poll on the listening sockets failed: {e}");
            return;
        }
        if rc == 0 {
            continue;
        }
        for (i, pfd) in pfds.iter().enumerate() {
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let (stream, peer) = match listeners[i].accept() {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept failed: {e}");
                    continue;
                }
            };
            if sessions.load(Ordering::Relaxed) >= MAX_SESSIONS {
                warn!("refusing {peer}: {MAX_SESSIONS} connections already open");
                continue;
            }
            sessions.fetch_add(1, Ordering::Relaxed);
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(c) = stream.try_clone() {
                live.lock().unwrap().insert(id, c);
            }
            let source = Arc::clone(&source);
            let registry = Arc::clone(registry);
            let counter = Arc::clone(&sessions);
            let live_for_thread = Arc::clone(&live);
            let stop = Arc::clone(&stop);
            let spawned = thread::Builder::new()
                .name(format!("usbfwd {peer}"))
                .spawn(move || {
                    if let Err(e) = handle(stream, peer, source.as_ref(), &registry, &stop) {
                        warn!("{peer}: {e}");
                    }
                    live_for_thread.lock().unwrap().remove(&id);
                    counter.fetch_sub(1, Ordering::Relaxed);
                });
            if let Err(e) = spawned {
                error!("cannot spawn a session thread: {e}");
                live.lock().unwrap().remove(&id);
                sessions.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    drain(&sessions, &live);
}

/// Wait for in-flight sessions to tear themselves down.
///
/// Session threads are detached, so without this the process would exit while
/// one is mid-URB and the device would be left driverless. Shutting the
/// sockets down first turns a reader blocked in `read()` into an immediate EOF.
fn drain(sessions: &AtomicUsize, live: &Mutex<HashMap<u64, TcpStream>>) {
    if sessions.load(Ordering::Relaxed) == 0 {
        return;
    }
    info!(
        "waiting for {} session(s) to finish",
        sessions.load(Ordering::Relaxed)
    );
    for s in live.lock().unwrap().values() {
        let _ = s.shutdown(Shutdown::Both);
    }
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while sessions.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let left = sessions.load(Ordering::Relaxed);
    if left > 0 {
        warn!(
            "{left} session(s) did not finish within {DRAIN_TIMEOUT:?}; \
             the device may need replugging to rebind its drivers"
        );
    }
}

pub fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    source: &dyn DeviceSource,
    registry: &Arc<Registry>,
    stop: &Stop,
) -> std::io::Result<()> {
    // Without this, Nagle batches small writes and adds tens of milliseconds
    // to every input report. It is the single most important socket option
    // here and it has to be set before anything is written.
    stream.set_nodelay(true)?;
    tune_keepalive(&stream);
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

    match read_op_request(&mut stream)? {
        Some(OpRequest::DevList) => {
            let devs: Vec<_> = source.list()?.iter().map(|d| d.to_usbip()).collect();
            info!("{peer}: device list ({} exportable)", devs.len());
            stream.write_all(&op::encode_devlist_reply(&devs))?;
            Ok(())
        }
        Some(OpRequest::Import { busid }) => {
            // From here on the client drives the pace, so the handshake
            // timeout has to go or a long-idle import would be torn down.
            stream.set_read_timeout(None)?;
            import(stream, peer, &busid, source, registry, stop)
        }
        None => {
            warn!("{peer}: unsupported USB/IP request");
            Ok(())
        }
    }
}

fn import(
    mut stream: TcpStream,
    peer: SocketAddr,
    busid: &str,
    source: &dyn DeviceSource,
    registry: &Arc<Registry>,
    stop: &Stop,
) -> std::io::Result<()> {
    let refuse = |s: &mut TcpStream, why: &str| -> std::io::Result<()> {
        warn!("{peer}: refusing to export {busid}: {why}");
        s.write_all(&op::encode_import_reply(None))
    };

    // Only ever import something already on the exportable list. The bus id
    // arrived over the network and must not be able to name a device the
    // allow-list excludes, let alone reach outside sysfs.
    if !source.list()?.iter().any(|d| d.busid == busid) {
        return refuse(&mut stream, "not in the allow-list, or no longer present");
    }

    let Some(lease) = registry.acquire(busid) else {
        return refuse(&mut stream, "already imported by another client");
    };

    let dev = match source.open(busid) {
        Ok(d) => d,
        Err(e) => return refuse(&mut stream, &format!("cannot open it: {e}")),
    };

    // Evicts whatever is bound — hid-generic today, hid-steam once the kernel
    // knows the Ibex ids. This is also what takes the controller away from a
    // Steam client running on this machine, which otherwise fights for it.
    if let Err(e) = dev.claim_all() {
        return refuse(&mut stream, &format!("cannot claim its interfaces: {e}"));
    }

    let summary = dev.summary().clone();
    let (vid, pid) = summary.vid_pid();
    stream.write_all(&op::encode_import_reply(Some(&summary.to_usbip())))?;
    info!(
        "{peer}: exporting {} {vid:04x}:{pid:04x}, {} interfaces, {:?} speed",
        summary.busid,
        summary.interfaces.len(),
        summary.speed
    );

    let result = session::run(stream, Arc::clone(&dev), stop.as_ref());
    dev.release_all();
    drop(lease);
    result
}

fn tune_keepalive(sock: &TcpStream) {
    // A client that drops off the tailnet without closing would otherwise hold
    // the device until the process restarts. ~25 s to detect a dead peer.
    let fd = sock.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, v: libc::c_int| unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &v as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 10);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
}

pub fn resolve_binds(binds: &[Bind]) -> std::io::Result<Vec<IpAddr>> {
    let mut out: Vec<IpAddr> = Vec::new();
    for b in binds {
        match b {
            Bind::Addr(a) => out.push(*a),
            Bind::Any => {
                warn!(
                    "--bind any exposes unencrypted USB traffic to every network this \
                     machine is on; prefer the default tailnet-only bind"
                );
                out.push(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                out.push(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
            }
            Bind::Tailscale => {
                let found = netif::tailscale_addrs()?;
                if found.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "no Tailscale address found. Start tailscale, or pass \
                         --bind <addr> for a specific interface (or --bind any, \
                         which puts plaintext USB traffic on every network).",
                    ));
                }
                out.extend(found);
            }
        }
    }
    out.dedup();
    Ok(out)
}

fn start_mdns(args: &Config) -> Option<thread::JoinHandle<()>> {
    let host = netif::hostname();
    let name = args.name.clone().unwrap_or_else(|| host.clone());
    let addrs = netif::lan_v4_addrs().unwrap_or_default();
    let txt = vec![format!("allow={}", args.filter)];
    let responder = match mdns::Responder::new(&name, &host, args.port, addrs, txt) {
        Ok(r) => r,
        Err(e) => {
            warn!("mDNS disabled: {e}");
            return None;
        }
    };
    info!("advertising {name}.{} on the LAN", mdns::SERVICE);
    thread::Builder::new()
        .name("usbfwd-mdns".into())
        .spawn(move || {
            let _ = responder.announce();
            let mut last = Instant::now();
            while !signal::shutting_down() {
                if let Err(e) = responder.poll_once() {
                    warn!("mDNS: {e}");
                    break;
                }
                if last.elapsed() > Duration::from_secs(60) {
                    let _ = responder.announce();
                    last = Instant::now();
                }
            }
            responder.goodbye();
        })
        .ok()
}

pub fn print_list(filter: &DeviceFilter) -> std::io::Result<()> {
    let devs = enumerate::list(filter)?;
    if devs.is_empty() {
        println!("No device matches {filter}.");
        let all = enumerate::list_all()?;
        if !all.is_empty() {
            println!("\nOn this machine, but not allowed by the filter:");
            for d in all {
                let (v, p) = d.vid_pid();
                println!("  {:<10} {v:04x}:{p:04x}", d.busid);
            }
        }
        return Ok(());
    }
    println!(
        "{:<10} {:<10} {:<8} INTERFACES",
        "BUSID", "VID:PID", "SPEED"
    );
    for d in devs {
        let (v, p) = d.vid_pid();
        let ifs: Vec<String> = d
            .interfaces
            .iter()
            .map(|i| {
                format!(
                    "{}:{:02x}/{:02x}/{:02x}",
                    i.number, i.class, i.sub_class, i.protocol
                )
            })
            .collect();
        println!(
            "{:<10} {v:04x}:{p:04x}  {:<8} {}",
            d.busid,
            format!("{:?}", d.speed).to_lowercase(),
            ifs.join(" ")
        );
    }
    Ok(())
}
