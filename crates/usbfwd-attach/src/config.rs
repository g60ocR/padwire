//! The `usbfwd.toml` model.
//!
//! Static endpoints are the primary mechanism, not a fallback. Multicast does
//! not cross a tailnet, so on the transport this project is built for there is
//! nothing to discover — the host has to be told where the exporter lives.
//! Discovery is the option, and it is off.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use usb_backend::DeviceFilter;
use usbip_proto::USBIP_PORT;

use crate::toml::{self, Table, Value};

const SERVER_KEYS: &[&str] = &["name", "host", "port", "devices", "auto_attach"];
const DISCOVERY_KEYS: &[&str] = &["mdns", "mdns_timeout"];
const ATTACH_KEYS: &[&str] = &[
    "poll_interval",
    "retry_min",
    "retry_max",
    "connect_timeout",
    "usbip",
    "modprobe",
    "stop_after_first",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// A MagicDNS name, a plain hostname, or a literal address.
    pub host: String,
    pub port: u16,
    pub devices: DeviceFilter,
    pub auto_attach: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub mdns: bool,
    pub mdns_timeout: Duration,
}

impl Default for Discovery {
    fn default() -> Self {
        Discovery {
            // LAN convenience only; cannot work over Tailscale.
            mdns: false,
            mdns_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    /// How often to re-examine the world when everything is steady.
    pub poll_interval: Duration,
    /// Backoff bounds for a server that will not answer.
    pub retry_min: Duration,
    pub retry_max: Duration,
    pub connect_timeout: Duration,
    /// The `usbip` binary used to hand the socket to `vhci-hcd`.
    pub usbip: String,
    pub modprobe: bool,
    /// Stop after one device is attached. A second controller on the host is
    /// rarely what anyone wants, and the Deck's own controls should not be
    /// swept up by a wildcard.
    pub stop_after_first: bool,
}

impl Default for Attach {
    fn default() -> Self {
        Attach {
            poll_interval: Duration::from_secs(5),
            retry_min: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(3),
            usbip: "usbip".into(),
            modprobe: true,
            stop_after_first: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub servers: Vec<Server>,
    pub discovery: Discovery,
    pub attach: Attach,
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{}: {}", p.display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

fn bad<T>(message: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError {
        path: None,
        message: message.into(),
    })
}

/// Where to look when `--config` is not given, in order.
pub fn default_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/etc/usbfwd/usbfwd.toml")];
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        v.push(Path::new(&dir).join("usbfwd/usbfwd.toml"));
    } else if let Some(home) = std::env::var_os("HOME") {
        v.push(Path::new(&home).join(".config/usbfwd/usbfwd.toml"));
    }
    v.push(PathBuf::from("usbfwd.toml"));
    v
}

pub fn find_default() -> Option<PathBuf> {
    default_paths().into_iter().find(|p| p.is_file())
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError {
        path: Some(path.to_owned()),
        message: e.to_string(),
    })?;
    parse(&text).map_err(|mut e| {
        e.path = Some(path.to_owned());
        e
    })
}

pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let tables = toml::parse(text).map_err(|e| ConfigError {
        path: None,
        message: e.to_string(),
    })?;
    let mut cfg = Config::default();

    for t in &tables {
        match t.name.as_str() {
            "server" => cfg.servers.push(server_from(t)?),
            "discovery" => cfg.discovery = discovery_from(t)?,
            "attach" => cfg.attach = attach_from(t)?,
            "" => {
                return bad(format!(
                    "line {}: settings must live under [[server]], [discovery] or [attach]",
                    t.line.max(1)
                ))
            }
            other => return bad(format!("line {}: unknown section [{other}]", t.line)),
        }
    }

    if cfg.servers.is_empty() && !cfg.discovery.mdns {
        return bad(
            "no [[server]] entries and discovery is off, so there is nothing to attach to. \
             Add a [[server]] with the exporter's MagicDNS name.",
        );
    }
    for (i, s) in cfg.servers.iter().enumerate() {
        if cfg.servers[..i].iter().any(|o| o.name == s.name) {
            return bad(format!("two servers are both named `{}`", s.name));
        }
    }
    Ok(cfg)
}

fn reject_unknown(t: &Table, known: &[&str]) -> Result<(), ConfigError> {
    // A typo in a key would otherwise mean the setting silently does nothing,
    // which for `host` or `devices` is a long debugging session.
    if let Some((k, line)) = t.unknown_keys(known).into_iter().next() {
        return bad(format!(
            "line {line}: `{k}` is not a valid key in [{}]",
            t.name
        ));
    }
    Ok(())
}

fn string(t: &Table, key: &str) -> Result<Option<String>, ConfigError> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(v) => bad(format!(
            "line {}: `{key}` must be a string, not a {}",
            t.line_of(key),
            v.type_name()
        )),
    }
}

fn integer(t: &Table, key: &str) -> Result<Option<i64>, ConfigError> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::Int(i)) => Ok(Some(*i)),
        Some(v) => bad(format!(
            "line {}: `{key}` must be an integer, not a {}",
            t.line_of(key),
            v.type_name()
        )),
    }
}

fn boolean(t: &Table, key: &str) -> Result<Option<bool>, ConfigError> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(v) => bad(format!(
            "line {}: `{key}` must be true or false, not a {}",
            t.line_of(key),
            v.type_name()
        )),
    }
}

fn seconds(t: &Table, key: &str, default: Duration) -> Result<Duration, ConfigError> {
    match integer(t, key)? {
        None => Ok(default),
        Some(n) if n >= 0 => Ok(Duration::from_secs(n as u64)),
        Some(n) => bad(format!(
            "line {}: `{key}` cannot be negative ({n})",
            t.line_of(key)
        )),
    }
}

fn server_from(t: &Table) -> Result<Server, ConfigError> {
    reject_unknown(t, SERVER_KEYS)?;
    let host = string(t, "host")?.ok_or_else(|| ConfigError {
        path: None,
        message: format!("line {}: [[server]] needs a `host`", t.line),
    })?;
    if host.trim().is_empty() {
        return bad(format!("line {}: `host` is empty", t.line_of("host")));
    }
    let port = match integer(t, "port")? {
        None => USBIP_PORT,
        Some(p) if (1..=65535).contains(&p) => p as u16,
        Some(p) => {
            return bad(format!(
                "line {}: `port` {p} is out of range",
                t.line_of("port")
            ))
        }
    };
    let devices = match t.get("devices") {
        None => DeviceFilter::default(),
        Some(Value::Array(a)) => a.join(",").parse().map_err(|e: String| ConfigError {
            path: None,
            message: format!("line {}: {e}", t.line_of("devices")),
        })?,
        Some(Value::Str(s)) => s.parse().map_err(|e: String| ConfigError {
            path: None,
            message: format!("line {}: {e}", t.line_of("devices")),
        })?,
        Some(v) => {
            return bad(format!(
                "line {}: `devices` must be an array of vid:pid patterns, not a {}",
                t.line_of("devices"),
                v.type_name()
            ))
        }
    };
    Ok(Server {
        name: string(t, "name")?.unwrap_or_else(|| host.clone()),
        host,
        port,
        devices,
        auto_attach: boolean(t, "auto_attach")?.unwrap_or(true),
    })
}

fn discovery_from(t: &Table) -> Result<Discovery, ConfigError> {
    reject_unknown(t, DISCOVERY_KEYS)?;
    let d = Discovery::default();
    Ok(Discovery {
        mdns: boolean(t, "mdns")?.unwrap_or(d.mdns),
        mdns_timeout: seconds(t, "mdns_timeout", d.mdns_timeout)?,
    })
}

fn attach_from(t: &Table) -> Result<Attach, ConfigError> {
    reject_unknown(t, ATTACH_KEYS)?;
    let d = Attach::default();
    let a = Attach {
        poll_interval: seconds(t, "poll_interval", d.poll_interval)?,
        retry_min: seconds(t, "retry_min", d.retry_min)?,
        retry_max: seconds(t, "retry_max", d.retry_max)?,
        connect_timeout: seconds(t, "connect_timeout", d.connect_timeout)?,
        usbip: string(t, "usbip")?.unwrap_or(d.usbip),
        modprobe: boolean(t, "modprobe")?.unwrap_or(d.modprobe),
        stop_after_first: boolean(t, "stop_after_first")?.unwrap_or(d.stop_after_first),
    };
    if a.retry_max < a.retry_min {
        return bad(format!(
            "line {}: `retry_max` is smaller than `retry_min`",
            t.line_of("retry_max")
        ));
    }
    if a.poll_interval.is_zero() {
        return bad(format!(
            "line {}: `poll_interval` must be at least 1s",
            t.line_of("poll_interval")
        ));
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
# usbfwd.toml — static endpoints are primary; discovery is optional
[[server]]
name    = "steamdeck"
host    = "steamdeck.tailnet-name.ts.net"   # MagicDNS, or a raw IP
port    = 3240
devices = ["28de:1304", "28de:1305", "28de:*"]
auto_attach = true

[[server]]
name = "tablet"
host = "tablet.tailnet-name.ts.net"

[discovery]
mdns = false        # LAN convenience only; cannot work over Tailscale
"#;

    #[test]
    fn the_documented_example_parses() {
        let c = parse(EXAMPLE).expect("parse");
        assert_eq!(c.servers.len(), 2);
        assert_eq!(c.servers[0].name, "steamdeck");
        assert_eq!(c.servers[0].host, "steamdeck.tailnet-name.ts.net");
        assert_eq!(c.servers[0].port, 3240);
        assert!(c.servers[0].devices.matches(0x28de, 0x1304));
        assert!(c.servers[0].auto_attach);
        // The second server takes every default.
        assert_eq!(c.servers[1].port, USBIP_PORT);
        assert!(c.servers[1].auto_attach);
        assert!(c.servers[1].devices.matches(0x28de, 0x9999));
        assert!(!c.discovery.mdns);
        assert_eq!(c.attach, Attach::default());
    }

    #[test]
    fn a_minimal_config_is_enough() {
        let c = parse("[[server]]\nhost = \"deck\"\n").unwrap();
        assert_eq!(c.servers[0].name, "deck", "the name defaults to the host");
        assert_eq!(c.servers[0].port, 3240);
        assert!(c.servers[0].devices.matches(0x28de, 0x1304));
    }

    #[test]
    fn a_config_with_nothing_to_do_is_an_error() {
        let e = parse("[discovery]\nmdns = false\n").unwrap_err();
        assert!(e.to_string().contains("nothing to attach to"), "{e}");
        // ...unless discovery is on, in which case servers may be absent.
        assert!(parse("[discovery]\nmdns = true\n").is_ok());
    }

    #[test]
    fn typos_are_reported_rather_than_ignored() {
        let e = parse("[[server]]\nhost = \"deck\"\nhsot = \"x\"\n").unwrap_err();
        assert!(e.to_string().contains("hsot"), "{e}");
        let e = parse("[[server]]\nhost = \"deck\"\n[discovery]\nmnds = true\n").unwrap_err();
        assert!(e.to_string().contains("mnds"), "{e}");
        let e = parse("[nonsense]\nx = 1\n").unwrap_err();
        assert!(e.to_string().contains("unknown section"), "{e}");
    }

    #[test]
    fn wrong_types_name_the_key_and_the_line() {
        let e = parse("[[server]]\nhost = 42\n").unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
        assert!(e.to_string().contains("must be a string"), "{e}");

        let e = parse("[[server]]\nhost = \"a\"\nauto_attach = \"yes\"\n").unwrap_err();
        assert!(e.to_string().contains("true or false"), "{e}");

        let e = parse("[[server]]\nhost = \"a\"\nport = 70000\n").unwrap_err();
        assert!(e.to_string().contains("out of range"), "{e}");
    }

    #[test]
    fn a_server_without_a_host_is_rejected() {
        let e = parse("[[server]]\nname = \"deck\"\n").unwrap_err();
        assert!(e.to_string().contains("needs a `host`"), "{e}");
    }

    #[test]
    fn duplicate_server_names_are_rejected() {
        let e = parse("[[server]]\nname=\"a\"\nhost=\"x\"\n[[server]]\nname=\"a\"\nhost=\"y\"\n")
            .unwrap_err();
        assert!(e.to_string().contains("both named"), "{e}");
    }

    #[test]
    fn attach_overrides_apply_and_are_sanity_checked() {
        let c = parse(
            "[[server]]\nhost=\"d\"\n[attach]\npoll_interval = 2\nretry_max = 30\nusbip = \"/usr/bin/usbip\"\nstop_after_first = false\n",
        )
        .unwrap();
        assert_eq!(c.attach.poll_interval, Duration::from_secs(2));
        assert_eq!(c.attach.retry_max, Duration::from_secs(30));
        assert_eq!(c.attach.usbip, "/usr/bin/usbip");
        assert!(!c.attach.stop_after_first);

        let e = parse("[[server]]\nhost=\"d\"\n[attach]\nretry_min = 90\nretry_max = 30\n")
            .unwrap_err();
        assert!(e.to_string().contains("smaller than"), "{e}");
        let e = parse("[[server]]\nhost=\"d\"\n[attach]\npoll_interval = 0\n").unwrap_err();
        assert!(e.to_string().contains("at least 1s"), "{e}");
    }

    #[test]
    fn a_devices_string_is_accepted_as_well_as_an_array() {
        let c = parse("[[server]]\nhost=\"d\"\ndevices = \"28de:1304\"\n").unwrap();
        assert!(c.servers[0].devices.matches(0x28de, 0x1304));
        assert!(!c.servers[0].devices.matches(0x28de, 0x1305));
    }

    #[test]
    fn a_bad_device_pattern_names_its_line() {
        let e = parse("[[server]]\nhost=\"d\"\ndevices = [\"not-a-pattern\"]\n").unwrap_err();
        assert!(e.to_string().contains("line 3"), "{e}");
    }
}
