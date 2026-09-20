//! usbfwd-server — export USB devices over USB/IP from pure userspace.
//!
//! No kernel module is needed on this side, which is the whole reason the
//! project exists: SteamOS ships an immutable filesystem without `usbip-host`,
//! and Android has no root. Both expose usbfs, so both can export.

use std::process::ExitCode;

use usb_backend::usbfs::DEFAULT_MAX_TRANSFER;
use usb_backend::DeviceFilter;
use usbfwd_common::log::Level;
use usbfwd_common::{error, log};
use usbfwd_server::{print_list, run, Bind, Config};
use usbip_proto::USBIP_PORT;
#[cfg(test)]
use {
    std::net::{IpAddr, Ipv4Addr, Ipv6Addr},
    usbfwd_common::netif,
    usbfwd_server::resolve_binds,
};

const USAGE: &str = "\
usbfwd-server — export USB devices over USB/IP (userspace, no kernel module)

USAGE:
    usbfwd-server [OPTIONS]

OPTIONS:
    --bind <ADDR>        Address to listen on. May be repeated.
                         `tailscale` (the default) binds every tailnet address.
                         `any` binds 0.0.0.0 and :: — see the warning below.
    --port <PORT>        TCP port [default: 3240]
    --allow <PATTERNS>   Comma-separated vid:pid allow-list, `*` permitted
                         [default: 28de:*, i.e. every Valve device]
    --max-transfer <N>   Largest single transfer to accept, in bytes
                         [default: 1048576]
    --mdns               Advertise _usbip._tcp on the LAN. Off by default and
                         useless over Tailscale, which does not carry multicast.
    --name <NAME>        mDNS instance name [default: the hostname]
    --prefetch           Keep an interrupt IN URB queued on the device, so a
                         client's submit is answered from input captured
                         before it arrived. Roughly halves how old a report is
                         by the time it lands, which is worth having on a link
                         with a round trip above a few milliseconds. Changes
                         what a submit means, so it is off by default.
    --list               Print the exportable devices and exit
    -v, --verbose        Raise the log level (repeatable)
    -q, --quiet          Errors only
    -V, --version        Print the version and exit
    -h, --help           Print this help

Port 3240 carries unencrypted USB traffic. The default bind keeps it on the
tailnet, where Tailscale supplies the encryption and identity that USB/IP has
none of. `--bind any` exposes it to every network the machine is attached to.

ENVIRONMENT:
    USBFWD_LOG           error | warn | info | debug | trace
";

struct Args {
    cfg: Config,
    list: bool,
    level: Option<Level>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            cfg: Config {
                // Filled in by parse_args; empty means "the caller said
                // nothing", which becomes the tailnet default.
                binds: Vec::new(),
                port: USBIP_PORT,
                filter: DeviceFilter::default(),
                max_transfer: DEFAULT_MAX_TRANSFER,
                mdns: false,
                name: None,
                prefetch: false,
            },
            list: false,
            level: None,
        }
    }
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut a = Args::default();
    let mut verbosity = 0i32;
    let mut it = argv.into_iter().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("usbfwd-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--bind" => {
                let v = value("--bind")?;
                a.cfg.binds.push(match v.as_str() {
                    "tailscale" | "auto" => Bind::Tailscale,
                    "any" | "all" => Bind::Any,
                    s => Bind::Addr(
                        s.parse()
                            .map_err(|_| format!("`{s}` is not an IP address"))?,
                    ),
                });
            }
            "--port" => {
                let v = value("--port")?;
                a.cfg.port = v.parse().map_err(|_| format!("`{v}` is not a port"))?;
            }
            "--allow" => a.cfg.filter = value("--allow")?.parse()?,
            "--max-transfer" => {
                let v = value("--max-transfer")?;
                a.cfg.max_transfer = v.parse().map_err(|_| format!("`{v}` is not a size"))?;
            }
            "--mdns" => a.cfg.mdns = true,
            "--prefetch" => a.cfg.prefetch = true,
            "--name" => a.cfg.name = Some(value("--name")?),
            "--list" => a.list = true,
            "--verbose" => verbosity += 1,
            // Accept -v, -vv, -vvv as one cluster.
            s if s.len() >= 2 && s.starts_with('-') && s[1..].chars().all(|c| c == 'v') => {
                verbosity += (s.len() - 1) as i32
            }
            "-q" | "--quiet" => verbosity -= 2,
            other => return Err(format!("unknown option `{other}` (try --help)")),
        }
    }
    if a.cfg.binds.is_empty() {
        a.cfg.binds.push(Bind::Tailscale);
    }
    a.level = match verbosity {
        i32::MIN..=-1 => Some(Level::Error),
        0 => None,
        1 => Some(Level::Debug),
        _ => Some(Level::Trace),
    };
    Ok(Some(a))
}

fn main() -> ExitCode {
    log::from_env();
    let args = match parse_args(std::env::args().collect()) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("usbfwd-server: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(l) = args.level {
        log::set_level(l);
    }
    let result = if args.list {
        print_list(&args.cfg.filter)
    } else {
        run(args.cfg)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<Args>, String> {
        let mut a = vec!["usbfwd-server".to_string()];
        a.extend(v.iter().map(|s| s.to_string()));
        parse_args(a)
    }

    #[test]
    fn defaults_are_tailnet_only_and_valve_only() {
        let a = args(&[]).unwrap().unwrap();
        assert_eq!(a.cfg.binds, vec![Bind::Tailscale]);
        assert_eq!(a.cfg.port, 3240);
        assert!(a.cfg.filter.matches(0x28de, 0x1304));
        assert!(!a.cfg.filter.matches(0x046d, 0x0001));
        assert!(!a.cfg.mdns, "mDNS must be opt-in");
    }

    #[test]
    fn binds_accumulate() {
        let a = args(&["--bind", "100.1.2.3", "--bind", "127.0.0.1"])
            .unwrap()
            .unwrap();
        assert_eq!(
            a.cfg.binds,
            vec![
                Bind::Addr("100.1.2.3".parse().unwrap()),
                Bind::Addr("127.0.0.1".parse().unwrap())
            ]
        );
    }

    #[test]
    fn any_expands_to_both_families() {
        let out = resolve_binds(&[Bind::Any]).unwrap();
        assert_eq!(
            out,
            vec![
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            ]
        );
    }

    #[test]
    fn a_missing_tailnet_is_an_actionable_error() {
        // On a machine without Tailscale this must explain the way out rather
        // than silently binding something exposed.
        if netif::tailscale_addrs().unwrap_or_default().is_empty() {
            let e = resolve_binds(&[Bind::Tailscale]).unwrap_err();
            assert!(e.to_string().contains("--bind"), "unhelpful: {e}");
        }
    }

    #[test]
    fn bad_options_are_rejected() {
        assert!(args(&["--port", "not-a-port"]).is_err());
        assert!(args(&["--bind", "not-an-ip"]).is_err());
        assert!(args(&["--allow", "nonsense"]).is_err());
        assert!(args(&["--frobnicate"]).is_err());
        assert!(args(&["--port"]).is_err(), "a missing value must not panic");
    }

    #[test]
    fn verbosity_maps_to_levels() {
        assert_eq!(args(&[]).unwrap().unwrap().level, None);
        assert_eq!(args(&["-v"]).unwrap().unwrap().level, Some(Level::Debug));
        assert_eq!(args(&["-vv"]).unwrap().unwrap().level, Some(Level::Trace));
        assert_eq!(
            args(&["-v", "-v"]).unwrap().unwrap().level,
            Some(Level::Trace)
        );
        assert_eq!(args(&["-q"]).unwrap().unwrap().level, Some(Level::Error));
    }
}
