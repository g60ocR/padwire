//! usbfwd-attach — keep the configured controller attached to this host.
//!
//! This is the piece that makes the whole thing usable rather than a party
//! trick. USB/IP does not recover from a network stall: it drops the device.
//! Over a tailnet, with a handheld that sleeps and wakes and roams between
//! networks, that happens routinely. So the daemon watches `vhci-hcd`, notices
//! when the device goes away, and reconnects — treating "the exporter is off"
//! as the normal state rather than an error, because most of the time the Deck
//! is in a bag.

mod client;
mod config;
mod toml;
mod vhci;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use config::{Config, Server};
use usbfwd_common::log::Level;
use usbfwd_common::{debug, error, info, log, mdns, signal, warn};
use usbip_proto::UsbDevice;

const USAGE: &str = "\
usbfwd-attach — keep configured USB/IP devices attached to this host

USAGE:
    usbfwd-attach [OPTIONS]

OPTIONS:
    -c, --config <PATH>  Configuration file. Without it, the first of:
                           /etc/usbfwd/usbfwd.toml
                           $XDG_CONFIG_HOME/usbfwd/usbfwd.toml
                           ./usbfwd.toml
        --once           Make one attempt at each server, then exit
        --status         Show what is attached right now, then exit
        --list           Ask each configured server what it has, then exit
        --detach-all     Detach every vhci port, then exit
    -v, --verbose        Raise the log level (repeatable)
    -q, --quiet          Errors only
    -V, --version        Print the version and exit
    -h, --help           Print this help

Attaching writes to sysfs, so this needs root. It is meant to run as a system
service; see packaging/usbfwd-attach.service.

ENVIRONMENT:
    USBFWD_LOG           error | warn | info | debug | trace
";

/// How long to wait for a freshly attached port to show up.
///
/// Enumeration is a dozen control transfers, so it costs a dozen round trips:
/// a few hundred milliseconds on loopback, but well over a second across a
/// tailnet to another city. Three seconds was measured against 127.0.0.1 and
/// is not generous anywhere else.
const PORT_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Daemon,
    Once,
    Status,
    List,
    DetachAll,
}

struct Args {
    config: Option<PathBuf>,
    mode: Mode,
    level: Option<Level>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut config = None;
    let mut mode = Mode::Daemon;
    let mut verbosity = 0i32;
    let mut it = argv.into_iter().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("usbfwd-attach {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => {
                config = Some(PathBuf::from(it.next().ok_or("--config needs a path")?))
            }
            "--once" => mode = Mode::Once,
            "--status" => mode = Mode::Status,
            "--list" => mode = Mode::List,
            "--detach-all" => mode = Mode::DetachAll,
            "--verbose" => verbosity += 1,
            "-q" | "--quiet" => verbosity -= 2,
            s if s.len() >= 2 && s.starts_with('-') && s[1..].chars().all(|c| c == 'v') => {
                verbosity += (s.len() - 1) as i32
            }
            other => return Err(format!("unknown option `{other}` (try --help)")),
        }
    }
    let level = match verbosity {
        i32::MIN..=-1 => Some(Level::Error),
        0 => None,
        1 => Some(Level::Debug),
        _ => Some(Level::Trace),
    };
    Ok(Some(Args {
        config,
        mode,
        level,
    }))
}

fn main() -> ExitCode {
    log::from_env();
    let args = match parse_args(std::env::args().collect()) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("usbfwd-attach: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(l) = args.level {
        log::set_level(l);
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    // --status and --detach-all are about the local kernel only, so they work
    // without a configuration file at all.
    match args.mode {
        Mode::Status => return print_status().map_err(Into::into),
        Mode::DetachAll => {
            let cfg = load_config(args.config.as_deref()).unwrap_or_default();
            return detach_all(&cfg).map_err(Into::into);
        }
        _ => {}
    }

    let cfg = load_config(args.config.as_deref())?;

    if args.mode == Mode::List {
        return list_remote(&cfg).map_err(Into::into);
    }

    signal::install();

    if cfg.attach.modprobe {
        if let Err(e) = vhci::modprobe() {
            // Not fatal on its own: the module may load later, or be built in
            // under a name modprobe does not know.
            warn!("{e}");
        }
    }
    if !vhci::loaded() {
        return Err("vhci-hcd is not loaded, so nothing can be attached. \
             Try `modprobe vhci-hcd` (Fedora: `dnf install kernel-modules-extra` first)."
            .into());
    }

    let mut daemon = Daemon::new(cfg);
    daemon.adopt_existing();

    if args.mode == Mode::Once {
        daemon.tick();
        return Ok(());
    }

    info!(
        "watching {} server(s); a server that is switched off is normal and will be retried quietly",
        daemon.cfg.servers.len()
    );
    while !signal::shutting_down() {
        daemon.tick();
        nap(daemon.cfg.attach.poll_interval);
    }
    info!("shutting down; leaving attached devices in place");
    Ok(())
}

fn load_config(explicit: Option<&std::path::Path>) -> Result<Config, Box<dyn std::error::Error>> {
    match explicit {
        Some(p) => Ok(config::load(p)?),
        None => match config::find_default() {
            Some(p) => {
                debug!("using {}", p.display());
                Ok(config::load(&p)?)
            }
            None => Err(format!(
                "no configuration file found. Looked in:\n{}\n\
                 Copy packaging/usbfwd.toml to the first of those and set the \
                 exporter's MagicDNS name.",
                config::default_paths()
                    .iter()
                    .map(|p| format!("  {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
            .into()),
        },
    }
}

/// One attachment this daemon is responsible for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    server: String,
    host: String,
    busid: String,
    port: u32,
}

struct Backoff {
    next_try: Instant,
    delay: Duration,
    /// Whether the last failure has already been reported at `info`.
    reported: bool,
}

struct Daemon {
    cfg: Config,
    attached: Vec<Tracked>,
    backoff: HashMap<String, Backoff>,
}

impl Daemon {
    fn new(cfg: Config) -> Daemon {
        Daemon {
            cfg,
            attached: Vec::new(),
            backoff: HashMap::new(),
        }
    }

    /// Pick up attachments that survived a restart of this daemon, so a
    /// `systemctl restart` does not attach a second copy of the controller.
    fn adopt_existing(&mut self) {
        let used = vhci::occupied_ports().unwrap_or_default();
        let mut identified: Vec<u32> = Vec::new();
        for r in vhci::records() {
            if !used.contains(&r.port) {
                continue;
            }
            identified.push(r.port);
            let server = self
                .cfg
                .servers
                .iter()
                .find(|s| s.host.eq_ignore_ascii_case(&r.host))
                .map(|s| s.name.clone())
                .unwrap_or_else(|| r.host.clone());
            info!(
                "adopting the existing attachment of {} from {} on vhci port {}",
                r.busid, r.host, r.port
            );
            self.attached.push(Tracked {
                server,
                host: r.host,
                busid: r.busid,
                port: r.port,
            });
        }

        // Ports that are busy but that no record explains. `usbip attach`
        // writes its records to a root-only directory and some builds do not
        // write them at all, so this is the common case rather than the odd
        // one. Adopt them anyway: an unidentified attachment is still a reason
        // not to attach a second controller on top of it.
        for port in used {
            if identified.contains(&port) {
                continue;
            }
            warn!(
                "vhci port {port} is busy but usbfwd cannot tell what is on it; \
                 leaving it alone. Use `usbfwd-attach --detach-all` to clear it."
            );
            self.attached.push(Tracked {
                server: "unknown".into(),
                host: "unknown".into(),
                busid: format!("vhci:{port}"),
                port,
            });
        }
    }

    /// True when there is nothing left to do this tick.
    fn satisfied(&self) -> bool {
        self.cfg.attach.stop_after_first && !self.attached.is_empty()
    }

    fn tick(&mut self) {
        self.prune();
        if self.satisfied() {
            return;
        }
        for server in self.endpoints() {
            if !server.auto_attach {
                continue;
            }
            if self.try_server(&server) && self.cfg.attach.stop_after_first {
                return;
            }
        }
    }

    /// Configured servers first; discovered ones appended only if enabled.
    fn endpoints(&self) -> Vec<Server> {
        let mut v = self.cfg.servers.clone();
        if self.cfg.discovery.mdns {
            match mdns::browse(self.cfg.discovery.mdns_timeout) {
                Ok(found) => {
                    for d in found {
                        if v.iter().any(|s| s.host.eq_ignore_ascii_case(&d.endpoint())) {
                            continue;
                        }
                        debug!("mDNS found {} at {}:{}", d.instance, d.endpoint(), d.port);
                        v.push(Server {
                            name: format!("mdns:{}", d.instance),
                            host: d.endpoint(),
                            port: d.port,
                            devices: Default::default(),
                            auto_attach: true,
                        });
                    }
                }
                Err(e) => debug!("mDNS browse failed: {e}"),
            }
        }
        v
    }

    /// Forget attachments whose vhci port is no longer in use — the usual sign
    /// that the exporter went away or the network dropped.
    fn prune(&mut self) {
        let ports = match vhci::ports() {
            Ok(p) => p,
            Err(e) => {
                warn!("cannot read vhci status: {e}");
                return;
            }
        };
        let usbip = self.cfg.attach.usbip.clone();
        let mut broken = Vec::new();
        self.attached.retain(|t| {
            match ports.iter().find(|p| p.port == t.port) {
                Some(p) if p.failed() => {
                    // A port stuck in `error` never frees itself, and the next
                    // attach would take a different one and leak this.
                    warn!(
                        "{} from {} left vhci port {} in an error state; detaching it",
                        t.busid, t.server, t.port
                    );
                    broken.push(t.port);
                    false
                }
                Some(p) if p.occupied() => true,
                _ => {
                    info!(
                        "{} from {} dropped off vhci port {}; will reattach",
                        t.busid, t.server, t.port
                    );
                    false
                }
            }
        });
        for port in broken {
            if let Err(e) = vhci::detach(&usbip, port) {
                warn!("could not detach port {port}: {e}");
            }
        }
    }

    /// Returns true if something was attached.
    fn try_server(&mut self, server: &Server) -> bool {
        if let Some(b) = self.backoff.get(&server.name) {
            if Instant::now() < b.next_try {
                return false;
            }
        }

        let devices =
            match client::devlist(&server.host, server.port, self.cfg.attach.connect_timeout) {
                Ok(d) => d,
                Err(e) => {
                    self.record_failure(server, &e.to_string());
                    return false;
                }
            };
        self.clear_failure(server);

        let candidate = devices.iter().find(|d| {
            server.devices.matches(d.id_vendor, d.id_product)
                && !self
                    .attached
                    .iter()
                    .any(|t| t.server == server.name && t.busid == d.busid)
        });
        let Some(dev) = candidate else {
            debug!(
                "{}: nothing matching {} among {} device(s)",
                server.name,
                server.devices,
                devices.len()
            );
            return false;
        };

        self.attach(server, dev)
    }

    fn attach(&mut self, server: &Server, dev: &UsbDevice) -> bool {
        let before = vhci::occupied_ports().unwrap_or_default();
        info!(
            "attaching {} from {} ({}:{})",
            client::describe(dev),
            server.name,
            server.host,
            server.port
        );
        if let Err(e) = vhci::attach(
            &self.cfg.attach.usbip,
            &server.host,
            server.port,
            &dev.busid,
        ) {
            warn!("{}: attach failed: {e}", server.name);
            self.record_failure(server, "attach failed");
            return false;
        }
        // The sysfs write marks the port NOTASSIGNED immediately, but nothing
        // else happens until the virtual root hub runs a port reset and
        // enumerates the device. Measured at roughly 300 ms; poll rather than
        // sleep a guessed amount or read too early and conclude nothing
        // happened.
        let deadline = Instant::now() + PORT_SETTLE_TIMEOUT;
        let mut found = None;
        while Instant::now() < deadline {
            let after = vhci::occupied_ports().unwrap_or_default();
            if let Some(p) = vhci::newly_used(&before, &after) {
                found = Some(p);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let Some(port) = found else {
            // The attach reported success but no port lit up. Do not record it,
            // or the next tick would think a device is attached when it is not.
            warn!(
                "{}: usbip reported success but no vhci port became busy within {:?}",
                server.name, PORT_SETTLE_TIMEOUT
            );
            return false;
        };
        info!(
            "{} is attached on vhci port {port}; check `lsusb -t` for a vhci_hcd bus",
            dev.busid
        );
        self.attached.push(Tracked {
            server: server.name.clone(),
            host: server.host.clone(),
            busid: dev.busid.clone(),
            port,
        });
        true
    }

    fn record_failure(&mut self, server: &Server, why: &str) {
        let min = self.cfg.attach.retry_min;
        let max = self.cfg.attach.retry_max;
        let e = self
            .backoff
            .entry(server.name.clone())
            .or_insert_with(|| Backoff {
                next_try: Instant::now(),
                delay: min,
                reported: false,
            });
        if !e.reported {
            // Say it once at a level people see, then stop: the exporter being
            // switched off is the normal state, not a fault.
            info!(
                "{} is not reachable yet ({why}); retrying quietly",
                server.name
            );
            e.reported = true;
        } else {
            debug!(
                "{} still unreachable ({why}); next try in {:?}",
                server.name, e.delay
            );
        }
        e.next_try = Instant::now() + e.delay;
        e.delay = (e.delay * 2).min(max);
    }

    fn clear_failure(&mut self, server: &Server) {
        if let Some(b) = self.backoff.remove(&server.name) {
            if b.reported {
                info!("{} is reachable again", server.name);
            }
        }
    }
}

fn nap(total: Duration) {
    let end = Instant::now() + total;
    while !signal::shutting_down() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

fn print_status() -> std::io::Result<()> {
    if !vhci::loaded() {
        println!("vhci-hcd is not loaded; nothing can be attached.");
        return Ok(());
    }
    let ports = vhci::ports()?;
    let records = vhci::records();
    let used = ports.iter().filter(|p| p.occupied()).count();
    println!("{} vhci port(s), {used} in use", ports.len());
    if used > 0 && records.is_empty() {
        match vhci::records_readable() {
            // The directory is mode 0700 root, so this is what an
            // unprivileged --status normally hits.
            Some(false) => println!("(run as root to see which remote each port came from)"),
            _ => println!("(usbip kept no record of these attachments; the remote is unknown)"),
        }
    }
    if used == 0 {
        // Still worth showing anything in an unexpected state: a port stuck
        // in `error` is why a reattach keeps failing.
        for p in ports.iter().filter(|p| p.status != vhci::VDEV_ST_NULL) {
            println!("  port {} is {}", p.port, p.status_name());
        }
        return Ok(());
    }
    println!(
        "{:<6} {:<12} {:<22} {:<10} LOCAL",
        "PORT", "STATE", "REMOTE", "BUSID"
    );
    for p in ports.iter().filter(|p| p.status != vhci::VDEV_ST_NULL) {
        let r = records.iter().find(|r| r.port == p.port);
        println!(
            "{:<6} {:<12} {:<22} {:<10} {}",
            p.port,
            p.status_name(),
            r.map(|r| format!("{}:{}", r.host, r.service_port))
                .unwrap_or_else(|| "?".into()),
            r.map(|r| r.busid.clone()).unwrap_or_else(|| "?".into()),
            p.local_busid
        );
    }
    Ok(())
}

fn detach_all(cfg: &Config) -> std::io::Result<()> {
    if !vhci::loaded() {
        println!("vhci-hcd is not loaded; nothing to detach.");
        return Ok(());
    }
    let mut n = 0;
    for p in vhci::ports()?.into_iter().filter(|p| p.in_use()) {
        match vhci::detach(&cfg.attach.usbip, p.port) {
            Ok(()) => {
                println!("detached port {}", p.port);
                n += 1;
            }
            Err(e) => eprintln!("port {}: {e}", p.port),
        }
    }
    if n == 0 {
        println!("nothing was attached.");
    }
    Ok(())
}

fn list_remote(cfg: &Config) -> std::io::Result<()> {
    for s in &cfg.servers {
        println!("{} ({}:{})", s.name, s.host, s.port);
        match client::devlist(&s.host, s.port, cfg.attach.connect_timeout) {
            Ok(devs) if devs.is_empty() => println!("  (exports nothing)"),
            Ok(devs) => {
                for d in devs {
                    let matched = if s.devices.matches(d.id_vendor, d.id_product) {
                        "*"
                    } else {
                        " "
                    };
                    println!("  {matched} {}", client::describe(&d));
                }
                println!("  (* = matches `devices = {}`)", s.devices);
            }
            Err(e) => println!("  unreachable: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<Args>, String> {
        let mut a = vec!["usbfwd-attach".to_string()];
        a.extend(v.iter().map(|s| s.to_string()));
        parse_args(a)
    }

    #[test]
    fn the_default_mode_is_the_daemon_loop() {
        let a = args(&[]).unwrap().unwrap();
        assert_eq!(a.mode, Mode::Daemon);
        assert!(a.config.is_none());
    }

    #[test]
    fn modes_and_config_parse() {
        assert_eq!(args(&["--once"]).unwrap().unwrap().mode, Mode::Once);
        assert_eq!(args(&["--status"]).unwrap().unwrap().mode, Mode::Status);
        assert_eq!(args(&["--list"]).unwrap().unwrap().mode, Mode::List);
        assert_eq!(
            args(&["-c", "/tmp/x.toml"]).unwrap().unwrap().config,
            Some(PathBuf::from("/tmp/x.toml"))
        );
        assert_eq!(args(&["-vv"]).unwrap().unwrap().level, Some(Level::Trace));
    }

    #[test]
    fn bad_options_are_rejected() {
        assert!(args(&["--nope"]).is_err());
        assert!(args(&["--config"]).is_err());
    }

    fn server(name: &str) -> Server {
        Server {
            name: name.into(),
            host: format!("{name}.example"),
            port: 3240,
            devices: Default::default(),
            auto_attach: true,
        }
    }

    fn daemon() -> Daemon {
        Daemon::new(Config {
            servers: vec![server("deck")],
            ..Default::default()
        })
    }

    #[test]
    fn backoff_doubles_up_to_the_ceiling_and_reports_once() {
        let mut d = daemon();
        d.cfg.attach.retry_min = Duration::from_secs(2);
        d.cfg.attach.retry_max = Duration::from_secs(16);
        let s = server("deck");

        d.record_failure(&s, "refused");
        assert!(d.backoff["deck"].reported, "the first failure is announced");
        assert_eq!(d.backoff["deck"].delay, Duration::from_secs(4));

        for expected in [8u64, 16, 16, 16] {
            d.record_failure(&s, "refused");
            assert_eq!(d.backoff["deck"].delay, Duration::from_secs(expected));
        }
    }

    #[test]
    fn success_clears_the_backoff() {
        let mut d = daemon();
        d.record_failure(&server("deck"), "refused");
        assert!(d.backoff.contains_key("deck"));
        d.clear_failure(&server("deck"));
        assert!(!d.backoff.contains_key("deck"));
    }

    #[test]
    fn a_server_in_backoff_is_skipped_without_connecting() {
        let mut d = daemon();
        let s = server("deck");
        d.record_failure(&s, "refused");
        // next_try is in the future, so this must return immediately rather
        // than spending connect_timeout on a host that is switched off.
        let start = Instant::now();
        assert!(!d.try_server(&s));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    fn tracked(port: u32) -> Tracked {
        Tracked {
            server: "deck".into(),
            host: "deck.example".into(),
            busid: "3-12".into(),
            port,
        }
    }

    #[test]
    fn stop_after_first_means_one_device() {
        let mut d = daemon();
        assert!(
            d.cfg.attach.stop_after_first,
            "one controller is the default"
        );
        assert!(
            !d.satisfied(),
            "nothing attached yet, so there is work to do"
        );
        d.attached.push(tracked(1));
        assert!(d.satisfied(), "with a device attached, a tick does no work");

        d.cfg.attach.stop_after_first = false;
        assert!(!d.satisfied(), "opting out keeps looking for more devices");
    }

    #[test]
    fn an_attachment_whose_port_went_away_is_forgotten() {
        let mut d = daemon();
        // Port 4095 cannot be in use: vhci exposes far fewer ports than that.
        d.attached.push(tracked(4095));
        d.prune();
        assert!(
            d.attached.is_empty() || !vhci::loaded(),
            "a vanished port must be dropped so it gets reattached"
        );
    }

    #[test]
    fn mdns_endpoints_are_absent_unless_discovery_is_enabled() {
        let d = daemon();
        assert!(!d.cfg.discovery.mdns);
        assert_eq!(d.endpoints().len(), 1);
    }
}
