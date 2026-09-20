//! Talking to `vhci-hcd`.
//!
//! Reading is done straight from sysfs, which is stable and machine-readable.
//! Attaching shells out to `usbip`, because handing the connected socket to
//! the kernel means writing its *file descriptor number* into
//! `.../attach`, and reproducing that dance buys nothing while `usbip attach`
//! is a package away. If that ever proves limiting, the native version is a
//! socket, an `OP_REQ_IMPORT`, and one `write`.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

pub const PLATFORM_DIR: &str = "/sys/devices/platform";
/// Where `usbip attach` records what it connected, one file per port.
pub const STATE_DIR: &str = "/var/run/vhci_hcd";

/// `enum usbip_device_status`, the vhci half.
pub const VDEV_ST_NULL: u32 = 4;
pub const VDEV_ST_NOTASSIGNED: u32 = 5;
pub const VDEV_ST_USED: u32 = 6;
pub const VDEV_ST_ERROR: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    /// `hs` or `ss` — which virtual root hub the port belongs to.
    pub hub: String,
    pub port: u32,
    pub status: u32,
    pub speed: u32,
    pub devid: u32,
    pub sockfd: u32,
    /// The device's name on *this* machine once attached, e.g. `5-1`. It is
    /// not the remote bus id; see [`records`] for that.
    pub local_busid: String,
}

impl Port {
    pub fn in_use(&self) -> bool {
        self.status == VDEV_ST_USED
    }

    /// Not free. A freshly attached port sits at `VDEV_ST_NOTASSIGNED` for a
    /// few hundred milliseconds — `vhci_hub_control` only promotes it to
    /// `VDEV_ST_USED` when the port reset during enumeration completes — so
    /// "in use" alone would report a device that is still coming up as absent.
    pub fn occupied(&self) -> bool {
        self.status != VDEV_ST_NULL
    }

    pub fn failed(&self) -> bool {
        self.status == VDEV_ST_ERROR
    }

    pub fn status_name(&self) -> &'static str {
        match self.status {
            VDEV_ST_NULL => "free",
            VDEV_ST_NOTASSIGNED => "not assigned",
            VDEV_ST_USED => "in use",
            VDEV_ST_ERROR => "error",
            _ => "unknown",
        }
    }
}

/// What `usbip attach` wrote about a port: the remote side of the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub port: u32,
    pub host: String,
    pub service_port: String,
    pub busid: String,
}

pub fn hcd_dir() -> Option<PathBuf> {
    let d = PathBuf::from(format!("{PLATFORM_DIR}/vhci_hcd.0"));
    d.is_dir().then_some(d)
}

pub fn loaded() -> bool {
    hcd_dir().is_some()
}

/// Load `vhci-hcd` if it is not already there. Needs root, and on Fedora the
/// module lives in `kernel-modules-extra`, which is a separate package.
pub fn modprobe() -> io::Result<()> {
    if loaded() {
        return Ok(());
    }
    let out = Command::new("modprobe")
        .arg("vhci-hcd")
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("running modprobe: {e}")))?;
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "modprobe vhci-hcd failed: {}. On Fedora the module is in \
                 kernel-modules-extra; on SteamOS it is built in.",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

pub fn parse_status(text: &str) -> Vec<Port> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        // The first line is a header; it is also the only one whose second
        // field is not a number.
        let (Ok(port), Ok(status), Ok(speed)) = (
            f[1].parse::<u32>(),
            f[2].parse::<u32>(),
            f[3].parse::<u32>(),
        ) else {
            continue;
        };
        out.push(Port {
            hub: f[0].to_owned(),
            port,
            status,
            speed,
            devid: u32::from_str_radix(f[4], 16).unwrap_or(0),
            sockfd: f[5].parse().unwrap_or(0),
            local_busid: f[6].to_owned(),
        });
    }
    out
}

/// Every vhci port. A multi-controller setup has `status`, `status.1`, ...
pub fn ports() -> io::Result<Vec<Port>> {
    let dir = hcd_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{PLATFORM_DIR}/vhci_hcd.0 is missing; vhci-hcd is not loaded"),
        )
    })?;
    let mut names: Vec<String> = fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n == "status" || n.starts_with("status."))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for n in names {
        if let Ok(text) = fs::read_to_string(dir.join(&n)) {
            out.extend(parse_status(&text));
        }
    }
    Ok(out)
}

/// Ports that are not free, including ones still being enumerated. This is the
/// set to diff across an attach.
pub fn occupied_ports() -> io::Result<Vec<u32>> {
    Ok(ports()?
        .into_iter()
        .filter(|p| p.occupied())
        .map(|p| p.port)
        .collect())
}

/// Whether the state directory `usbip attach` writes can be read.
///
/// `None` means it does not exist (nothing has attached since boot, or this
/// build of `usbip` does not keep records). `Some(false)` means it exists but
/// is not readable — it is mode 0700 root, so an unprivileged `--status` sees
/// this and should say so rather than printing blanks.
pub fn records_readable() -> Option<bool> {
    let p = std::path::Path::new(STATE_DIR);
    if !p.exists() {
        return None;
    }
    Some(fs::read_dir(p).is_ok())
}

/// Read back what `usbip attach` recorded. Best effort: the directory only
/// exists if something has attached since boot, and a native attach would not
/// write it at all.
pub fn records() -> Vec<Record> {
    let Ok(dir) = fs::read_dir(STATE_DIR) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(port) = name
            .strip_prefix("port")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(text) = fs::read_to_string(e.path()) else {
            continue;
        };
        let f: Vec<&str> = text.split_whitespace().collect();
        if f.len() >= 3 {
            out.push(Record {
                port,
                host: f[0].to_owned(),
                service_port: f[1].to_owned(),
                busid: f[2].to_owned(),
            });
        }
    }
    out.sort_by_key(|r| r.port);
    out
}

fn run(usbip: &str, args: &[String]) -> io::Result<String> {
    let out = Command::new(usbip).args(args).output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "running `{usbip} {}`: {e}. Install the usbip tools \
                 (Fedora: `dnf install usbip`) or set `usbip` in [attach].",
                args.join(" ")
            ),
        )
    })?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            if stderr.is_empty() {
                format!("`{usbip} {}` exited with {}", args.join(" "), out.status)
            } else {
                stderr
            },
        ));
    }
    Ok(stderr)
}

pub fn attach(usbip: &str, host: &str, port: u16, busid: &str) -> io::Result<()> {
    run(
        usbip,
        &[
            "--tcp-port".into(),
            port.to_string(),
            "attach".into(),
            format!("--remote={host}"),
            format!("--busid={busid}"),
        ],
    )
    .map(|_| ())
}

pub fn detach(usbip: &str, port: u32) -> io::Result<()> {
    run(usbip, &["detach".into(), format!("--port={port}")]).map(|_| ())
}

/// The port that appeared between two snapshots of [`occupied_ports`].
pub fn newly_used(before: &[u32], after: &[u32]) -> Option<u32> {
    after.iter().copied().find(|p| !before.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `/sys/devices/platform/vhci_hcd.0/status` with one
    /// device attached.
    const SAMPLE: &str = "\
hub port sta spd dev      sockfd local_busid
hs  0000 004 000 00000000 000000 0-0
hs  0001 006 002 00030003 000009 5-1
hs  0002 004 000 00000000 000000 0-0
ss  0008 004 000 00000000 000000 0-0
";

    #[test]
    fn the_header_line_is_skipped() {
        let p = parse_status(SAMPLE);
        assert_eq!(p.len(), 4, "got {p:?}");
        assert!(p.iter().all(|p| p.hub == "hs" || p.hub == "ss"));
    }

    #[test]
    fn a_used_port_is_decoded_fully() {
        let p = parse_status(SAMPLE);
        let used: Vec<&Port> = p.iter().filter(|p| p.in_use()).collect();
        assert_eq!(used.len(), 1);
        let u = used[0];
        assert_eq!(u.port, 1);
        assert_eq!(u.status, VDEV_ST_USED);
        assert_eq!(u.speed, 2);
        // devid is hex: bus 3, device 3.
        assert_eq!(u.devid, 0x0003_0003);
        assert_eq!(u.devid >> 16, 3);
        assert_eq!(u.devid & 0xffff, 3);
        assert_eq!(u.sockfd, 9);
        assert_eq!(u.local_busid, "5-1");
        assert_eq!(u.status_name(), "in use");
    }

    #[test]
    fn free_ports_are_not_reported_as_attached() {
        let p = parse_status(SAMPLE);
        assert_eq!(p.iter().filter(|p| !p.in_use()).count(), 3);
        assert_eq!(p[0].status_name(), "free");
    }

    #[test]
    fn garbage_lines_are_ignored_rather_than_panicking() {
        assert!(parse_status("").is_empty());
        assert!(parse_status("nonsense\n").is_empty());
        assert!(parse_status("hs 0001\n").is_empty());
        assert!(parse_status("hs  xxxx yyy zzz 00000000 000000 0-0\n").is_empty());
    }

    #[test]
    fn a_port_being_enumerated_counts_as_occupied() {
        // Measured on a real attach: vhci reports 005 (NOTASSIGNED) for about
        // 300 ms before promoting to 006 (USED). Treating only 006 as taken
        // makes an attach look like it did nothing.
        let mid_attach = "hs  0000 005 000 00000000 000000 0-0\n";
        let p = &parse_status(mid_attach)[0];
        assert!(!p.in_use(), "not finished enumerating yet");
        assert!(p.occupied(), "but the port is certainly not free");
        assert_eq!(p.status_name(), "not assigned");

        let free = &parse_status("hs  0000 004 000 00000000 000000 0-0\n")[0];
        assert!(!free.occupied());

        let broken = &parse_status("hs  0000 007 000 00000000 000000 0-0\n")[0];
        assert!(broken.occupied());
        assert!(broken.failed());
    }

    #[test]
    fn a_new_port_is_detected_by_difference() {
        assert_eq!(newly_used(&[0, 2], &[0, 1, 2]), Some(1));
        assert_eq!(newly_used(&[0, 1], &[0, 1]), None);
        assert_eq!(newly_used(&[], &[3]), Some(3));
    }

    /// Runs for real if vhci-hcd happens to be loaded on the build machine.
    #[test]
    fn reading_the_live_status_file_works_when_the_module_is_loaded() {
        if !loaded() {
            return;
        }
        let ports = ports().expect("read status");
        assert!(!ports.is_empty(), "vhci_hcd.0 exists but exposes no ports");
        assert!(ports.iter().all(|p| p.hub == "hs" || p.hub == "ss"));
        // Ports are numbered from zero and unique within a hub.
        let mut seen: Vec<(String, u32)> = ports.iter().map(|p| (p.hub.clone(), p.port)).collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), before, "duplicate hub/port pairs");
    }
}
