//! The client half of the USB/IP handshake.
//!
//! Only `OP_REQ_DEVLIST` is needed: the daemon uses it to find out what an
//! exporter has before deciding whether to attach, and `usbip attach` does the
//! import itself. Speaking the protocol directly here — rather than parsing
//! `usbip list -r` — means device matching happens on structured data.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use usbip_proto::io::read_devlist_reply;
use usbip_proto::{op, UsbDevice};

/// Connect, honouring `timeout` per address. A MagicDNS name resolves to both
/// an IPv4 and an IPv6 address; trying each in turn is what makes the daemon
/// work regardless of which one the tailnet prefers today.
pub fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot resolve {host}:{port}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host}:{port} resolved to no addresses"),
        ));
    }
    let mut last = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => {
                // The same reason the server sets it: without it, Nagle adds
                // tens of milliseconds to every small exchange.
                s.set_nodelay(true)?;
                s.set_read_timeout(Some(timeout))?;
                s.set_write_timeout(Some(timeout))?;
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("cannot connect to {host}:{port}"),
        )
    }))
}

/// Ask an exporter what it has.
pub fn devlist(host: &str, port: u16, timeout: Duration) -> io::Result<Vec<UsbDevice>> {
    use std::io::Write;
    let mut s = connect(host, port, timeout)?;
    s.write_all(&op::encode_devlist_request())?;
    read_devlist_reply(&mut s)
}

/// Format a device the way the rest of the daemon refers to it.
pub fn describe(d: &UsbDevice) -> String {
    format!(
        "{} ({:04x}:{:04x}, {} interfaces)",
        d.busid, d.id_vendor, d.id_product, d.b_num_interfaces
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use usbip_proto::UsbInterface;

    fn puck() -> UsbDevice {
        UsbDevice {
            busid: "3-12".into(),
            busnum: 3,
            devnum: 12,
            id_vendor: 0x28de,
            id_product: 0x1304,
            b_num_interfaces: 7,
            interfaces: vec![
                UsbInterface {
                    class: 3,
                    subclass: 0,
                    protocol: 0
                };
                7
            ],
            ..Default::default()
        }
    }

    #[test]
    fn devlist_round_trips_against_a_stub_exporter() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = [0u8; 8];
            s.read_exact(&mut req).unwrap();
            s.write_all(&op::encode_devlist_reply(&[puck()])).unwrap();
        });
        let got = devlist("127.0.0.1", addr.port(), Duration::from_secs(2)).unwrap();
        h.join().unwrap();
        assert_eq!(got, vec![puck()]);
        assert_eq!(describe(&got[0]), "3-12 (28de:1304, 7 interfaces)");
    }

    #[test]
    fn an_unreachable_server_errors_rather_than_hanging() {
        // Port 1 on loopback refuses immediately; the point is that the error
        // is returned, not that it takes any particular form.
        let e = devlist("127.0.0.1", 1, Duration::from_millis(500)).unwrap_err();
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn an_unresolvable_name_says_so() {
        let e = devlist(
            "this-host-does-not-exist.invalid",
            3240,
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(e.to_string().contains("resolve"), "{e}");
    }
}
