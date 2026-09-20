//! A single-service mDNS responder and a one-shot browser for `_usbip._tcp`.
//!
//! This is a LAN convenience and nothing more. Multicast does not cross a
//! tailnet, so over Tailscale — the transport usbfwd is actually built for —
//! none of this can work and static endpoints are the real mechanism. It stays
//! off by default on both sides and exists so that `avahi-browse -r
//! _usbip._tcp` finds an exporter when both machines are on the same LAN.
//!
//! Deliberately not implemented: name-conflict probing, known-answer
//! suppression, caching, and any service other than the one instance a process
//! advertises for itself.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::{FromRawFd, RawFd};
use std::time::{Duration, Instant};

pub const SERVICE: &str = "_usbip._tcp.local";
pub const MDNS_PORT: u16 = 5353;
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;

const CLASS_IN: u16 = 1;
/// Top bit of the class field: in a question it asks for a unicast reply, in a
/// record it tells receivers to flush what they had cached.
const FLAG_UNICAST_OR_FLUSH: u16 = 0x8000;

const TTL_HOST: u32 = 120;
const TTL_PTR: u32 = 4500;

const MAX_PACKET: usize = 9000;

// ---------------------------------------------------------------- wire format

fn encode_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let b = label.as_bytes();
        let n = b.len().min(63);
        out.push(n as u8);
        out.extend_from_slice(&b[..n]);
    }
    out.push(0);
}

/// Read a possibly-compressed name, advancing `pos` past it.
fn read_name(buf: &[u8], pos: &mut usize) -> Option<String> {
    let mut out = String::new();
    let mut p = *pos;
    let mut jumped = false;
    let mut hops = 0;
    loop {
        let len = *buf.get(p)?;
        if len & 0xc0 == 0xc0 {
            let lo = *buf.get(p + 1)? as usize;
            if !jumped {
                *pos = p + 2;
                jumped = true;
            }
            hops += 1;
            // A pointer loop would otherwise hang the responder.
            if hops > 16 {
                return None;
            }
            p = (((len & 0x3f) as usize) << 8) | lo;
            continue;
        }
        if len == 0 {
            if !jumped {
                *pos = p + 1;
            }
            return Some(out);
        }
        let len = len as usize;
        p += 1;
        let label = buf.get(p..p + len)?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(label));
        p += len;
    }
}

struct Message {
    buf: Vec<u8>,
    questions: u16,
    answers: u16,
    additional: u16,
}

impl Message {
    fn new(id: u16, flags: u16) -> Message {
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(&flags.to_be_bytes());
        buf.extend_from_slice(&[0; 8]); // counts, patched in finish()
        Message {
            buf,
            questions: 0,
            answers: 0,
            additional: 0,
        }
    }

    fn question(&mut self, name: &str, qtype: u16, qclass: u16) {
        encode_name(&mut self.buf, name);
        self.buf.extend_from_slice(&qtype.to_be_bytes());
        self.buf.extend_from_slice(&qclass.to_be_bytes());
        self.questions += 1;
    }

    fn record(&mut self, name: &str, rtype: u16, class: u16, ttl: u32, rdata: &[u8], extra: bool) {
        encode_name(&mut self.buf, name);
        self.buf.extend_from_slice(&rtype.to_be_bytes());
        self.buf.extend_from_slice(&class.to_be_bytes());
        self.buf.extend_from_slice(&ttl.to_be_bytes());
        self.buf
            .extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(rdata);
        if extra {
            self.additional += 1;
        } else {
            self.answers += 1;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.buf[4..6].copy_from_slice(&self.questions.to_be_bytes());
        self.buf[6..8].copy_from_slice(&self.answers.to_be_bytes());
        self.buf[10..12].copy_from_slice(&self.additional.to_be_bytes());
        self.buf
    }
}

fn ptr_rdata(name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    encode_name(&mut v, name);
    v
}

fn srv_rdata(port: u16, target: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0u16.to_be_bytes()); // priority
    v.extend_from_slice(&0u16.to_be_bytes()); // weight
    v.extend_from_slice(&port.to_be_bytes());
    encode_name(&mut v, target);
    v
}

fn txt_rdata(entries: &[String]) -> Vec<u8> {
    let mut v = Vec::new();
    if entries.is_empty() {
        v.push(0);
        return v;
    }
    for e in entries {
        let b = e.as_bytes();
        let n = b.len().min(255);
        v.push(n as u8);
        v.extend_from_slice(&b[..n]);
    }
    v
}

fn parse_txt(rdata: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < rdata.len() {
        let n = rdata[p] as usize;
        p += 1;
        if n == 0 || p + n > rdata.len() {
            break;
        }
        out.push(String::from_utf8_lossy(&rdata[p..p + n]).into_owned());
        p += n;
    }
    out
}

// ------------------------------------------------------------------- sockets

fn setsockopt_int(fd: RawFd, level: i32, name: i32, value: i32) -> io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &value as *const i32 as *const libc::c_void,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Bind a UDP socket, sharing port 5353 with an existing responder if there is
/// one. `std` cannot set `SO_REUSEPORT`, and without it a host already running
/// Avahi refuses the bind.
fn bind_udp(port: u16) -> io::Result<UdpSocket> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: `fd` is a fresh descriptor and ownership moves into the socket,
    // so it is closed exactly once even on the error paths below.
    let sock = unsafe { UdpSocket::from_raw_fd(fd) };
    setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)?;

    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    sock.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    sock.set_multicast_loop_v4(true)?;
    Ok(sock)
}

fn group_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT))
}

// ----------------------------------------------------------------- responder

pub struct Responder {
    sock: UdpSocket,
    /// `steamdeck._usbip._tcp.local`
    instance_fqdn: String,
    /// `steamdeck.local`
    host_fqdn: String,
    port: u16,
    addrs: Vec<Ipv4Addr>,
    txt: Vec<String>,
}

impl Responder {
    pub fn new(
        instance: &str,
        host: &str,
        port: u16,
        addrs: Vec<Ipv4Addr>,
        txt: Vec<String>,
    ) -> io::Result<Responder> {
        let sock = bind_udp(MDNS_PORT)?;
        sock.set_read_timeout(Some(Duration::from_millis(250)))?;
        Ok(Responder {
            sock,
            instance_fqdn: format!("{instance}.{SERVICE}"),
            host_fqdn: format!("{host}.local"),
            port,
            addrs,
            txt,
        })
    }

    fn answer(&self, include_ptr: bool, ttl_override: Option<u32>) -> Vec<u8> {
        let ttl_host = ttl_override.unwrap_or(TTL_HOST);
        let ttl_ptr = ttl_override.unwrap_or(TTL_PTR);
        let flush = CLASS_IN | FLAG_UNICAST_OR_FLUSH;
        let mut m = Message::new(0, 0x8400); // response, authoritative
        if include_ptr {
            m.record(
                SERVICE,
                TYPE_PTR,
                CLASS_IN,
                ttl_ptr,
                &ptr_rdata(&self.instance_fqdn),
                false,
            );
        }
        m.record(
            &self.instance_fqdn,
            TYPE_SRV,
            flush,
            ttl_host,
            &srv_rdata(self.port, &self.host_fqdn),
            false,
        );
        m.record(
            &self.instance_fqdn,
            TYPE_TXT,
            flush,
            ttl_host,
            &txt_rdata(&self.txt),
            false,
        );
        for a in &self.addrs {
            m.record(&self.host_fqdn, TYPE_A, flush, ttl_host, &a.octets(), true);
        }
        m.finish()
    }

    pub fn announce(&self) -> io::Result<()> {
        self.sock.send_to(&self.answer(true, None), group_addr())?;
        Ok(())
    }

    /// Withdraw the advertisement by re-announcing it with a zero TTL.
    pub fn goodbye(&self) {
        let _ = self.sock.send_to(&self.answer(true, Some(0)), group_addr());
    }

    /// Answer queries until `stop` returns true. Blocks for up to the socket's
    /// read timeout between checks.
    pub fn poll_once(&self) -> io::Result<()> {
        let mut buf = [0u8; MAX_PACKET];
        let (n, from) = match self.sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(())
            }
            Err(e) => return Err(e),
        };
        let Some((wants_ptr, wants_service, unicast)) = self.match_questions(&buf[..n]) else {
            return Ok(());
        };
        if !wants_ptr && !wants_service {
            return Ok(());
        }
        let reply = self.answer(wants_ptr, None);
        let dest = if unicast { from } else { group_addr() };
        self.sock.send_to(&reply, dest)?;
        Ok(())
    }

    /// Returns `(asked for the service type, asked about us specifically,
    /// wants a unicast reply)`.
    fn match_questions(&self, buf: &[u8]) -> Option<(bool, bool, bool)> {
        if buf.len() < 12 {
            return None;
        }
        let flags = u16::from_be_bytes([buf[2], buf[3]]);
        if flags & 0x8000 != 0 {
            return None; // a response, not a query
        }
        let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
        let mut pos = 12;
        let (mut ptr, mut svc, mut unicast) = (false, false, false);
        for _ in 0..qdcount {
            let name = read_name(buf, &mut pos)?;
            let qtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
            let qclass = u16::from_be_bytes([*buf.get(pos + 2)?, *buf.get(pos + 3)?]);
            pos += 4;
            if qclass & FLAG_UNICAST_OR_FLUSH != 0 {
                unicast = true;
            }
            let name = name.to_ascii_lowercase();
            if name == SERVICE && (qtype == TYPE_PTR || qtype == TYPE_ANY) {
                ptr = true;
            } else if (name == self.instance_fqdn.to_ascii_lowercase()
                && matches!(qtype, TYPE_SRV | TYPE_TXT | TYPE_ANY))
                || (name == self.host_fqdn.to_ascii_lowercase()
                    && matches!(qtype, TYPE_A | TYPE_ANY))
            {
                // Either "tell me about this instance" or "resolve this host";
                // both are answered with the same SRV + TXT + A set.
                svc = true;
            }
        }
        Some((ptr, svc, unicast))
    }
}

// ------------------------------------------------------------------- browser

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub instance: String,
    pub host: String,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
    pub txt: Vec<String>,
}

impl Discovered {
    /// The best endpoint to hand to `usbip attach`: a literal address if one
    /// was advertised, otherwise the `.local` name for the resolver to sort out.
    pub fn endpoint(&self) -> String {
        match self.addrs.first() {
            Some(a) => a.to_string(),
            None => self.host.clone(),
        }
    }
}

/// One-shot browse. Sends a single query and collects answers for `timeout`.
pub fn browse(timeout: Duration) -> io::Result<Vec<Discovered>> {
    // An ephemeral source port with the unicast-response bit set keeps this out
    // of the way of a system responder already holding 5353.
    let sock = bind_udp(0)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;

    let mut q = Message::new(0, 0);
    q.question(SERVICE, TYPE_PTR, CLASS_IN | FLAG_UNICAST_OR_FLUSH);
    sock.send_to(&q.finish(), group_addr())?;

    let deadline = Instant::now() + timeout;
    let mut instances: Vec<String> = Vec::new();
    let mut srv: Vec<(String, u16, String)> = Vec::new();
    let mut txts: Vec<(String, Vec<String>)> = Vec::new();
    let mut addrs: Vec<(String, IpAddr)> = Vec::new();
    let mut buf = [0u8; MAX_PACKET];

    while Instant::now() < deadline {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        collect(&buf[..n], &mut instances, &mut srv, &mut txts, &mut addrs);
    }

    let mut out = Vec::new();
    for inst in instances {
        let Some((_, port, target)) = srv.iter().find(|(n, _, _)| n == &inst).cloned() else {
            continue;
        };
        let ips: Vec<IpAddr> = addrs
            .iter()
            .filter(|(h, _)| h.eq_ignore_ascii_case(&target))
            .map(|(_, a)| *a)
            .collect();
        let txt = txts
            .iter()
            .find(|(n, _)| n == &inst)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        out.push(Discovered {
            instance: inst.trim_end_matches(&format!(".{SERVICE}")).to_owned(),
            host: target,
            port,
            addrs: ips,
            txt,
        });
    }
    out.dedup_by(|a, b| a.instance == b.instance);
    Ok(out)
}

fn collect(
    buf: &[u8],
    instances: &mut Vec<String>,
    srv: &mut Vec<(String, u16, String)>,
    txts: &mut Vec<(String, Vec<String>)>,
    addrs: &mut Vec<(String, IpAddr)>,
) -> Option<()> {
    if buf.len() < 12 {
        return None;
    }
    let counts: Vec<u16> = (4..12)
        .step_by(2)
        .map(|i| u16::from_be_bytes([buf[i], buf[i + 1]]))
        .collect();
    let mut pos = 12;
    for _ in 0..counts[0] {
        read_name(buf, &mut pos)?;
        pos = pos.checked_add(4)?;
    }
    let total = counts[1] as usize + counts[2] as usize + counts[3] as usize;
    for _ in 0..total {
        let name = read_name(buf, &mut pos)?;
        let rtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
        let rdlen = u16::from_be_bytes([*buf.get(pos + 8)?, *buf.get(pos + 9)?]) as usize;
        pos += 10;
        let rdata = buf.get(pos..pos + rdlen)?;
        match rtype {
            TYPE_PTR if name.eq_ignore_ascii_case(SERVICE) => {
                let mut p = pos;
                if let Some(target) = read_name(buf, &mut p) {
                    if !instances.iter().any(|i| i.eq_ignore_ascii_case(&target)) {
                        instances.push(target);
                    }
                }
            }
            TYPE_SRV if rdlen >= 7 => {
                let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                let mut p = pos + 6;
                if let Some(target) = read_name(buf, &mut p) {
                    srv.push((name.clone(), port, target));
                }
            }
            TYPE_TXT => txts.push((name.clone(), parse_txt(rdata))),
            TYPE_A if rdlen == 4 => addrs.push((
                name.clone(),
                IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            )),
            _ => {}
        }
        pos += rdlen;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let mut v = Vec::new();
        encode_name(&mut v, "steamdeck._usbip._tcp.local");
        assert_eq!(v[0], 9);
        let mut pos = 0;
        assert_eq!(
            read_name(&v, &mut pos).unwrap(),
            "steamdeck._usbip._tcp.local"
        );
        assert_eq!(pos, v.len());
    }

    #[test]
    fn compression_pointers_are_followed_and_bounded() {
        // "a.local" at offset 0, then a pointer back to it.
        let mut buf = Vec::new();
        encode_name(&mut buf, "a.local");
        let ptr_at = buf.len();
        buf.push(0xc0);
        buf.push(0x00);
        let mut pos = ptr_at;
        assert_eq!(read_name(&buf, &mut pos).unwrap(), "a.local");
        assert_eq!(pos, ptr_at + 2);

        // A pointer to itself must terminate rather than spin.
        let loop_buf = [0xc0u8, 0x00];
        let mut pos = 0;
        assert_eq!(read_name(&loop_buf, &mut pos), None);
    }

    #[test]
    fn truncated_packets_do_not_panic() {
        let mut v = Vec::new();
        encode_name(&mut v, "steamdeck.local");
        for cut in 0..v.len() {
            let mut pos = 0;
            let _ = read_name(&v[..cut], &mut pos);
        }
    }

    #[test]
    fn a_response_carries_ptr_srv_txt_and_a() {
        let r = Responder {
            sock: bind_udp(0).expect("socket"),
            instance_fqdn: format!("deck.{SERVICE}"),
            host_fqdn: "deck.local".into(),
            port: 3240,
            addrs: vec![Ipv4Addr::new(100, 101, 102, 103)],
            txt: vec!["devices=28de:1304".into()],
        };
        let pkt = r.answer(true, None);
        let mut instances = Vec::new();
        let mut srv = Vec::new();
        let mut txts = Vec::new();
        let mut addrs = Vec::new();
        collect(&pkt, &mut instances, &mut srv, &mut txts, &mut addrs).expect("parse");
        assert_eq!(instances, vec![format!("deck.{SERVICE}")]);
        assert_eq!(srv[0].1, 3240);
        assert_eq!(srv[0].2, "deck.local");
        assert_eq!(txts[0].1, vec!["devices=28de:1304".to_string()]);
        assert_eq!(addrs[0].1, IpAddr::V4(Ipv4Addr::new(100, 101, 102, 103)));
    }

    #[test]
    fn a_ptr_query_for_our_service_is_recognised() {
        let r = Responder {
            sock: bind_udp(0).expect("socket"),
            instance_fqdn: format!("deck.{SERVICE}"),
            host_fqdn: "deck.local".into(),
            port: 3240,
            addrs: vec![],
            txt: vec![],
        };
        let mut q = Message::new(0, 0);
        q.question(SERVICE, TYPE_PTR, CLASS_IN);
        assert_eq!(r.match_questions(&q.finish()), Some((true, false, false)));

        let mut q = Message::new(0, 0);
        q.question("_printer._tcp.local", TYPE_PTR, CLASS_IN);
        assert_eq!(r.match_questions(&q.finish()), Some((false, false, false)));

        // The unicast-response bit must be picked up.
        let mut q = Message::new(0, 0);
        q.question(SERVICE, TYPE_ANY, CLASS_IN | FLAG_UNICAST_OR_FLUSH);
        assert_eq!(r.match_questions(&q.finish()), Some((true, false, true)));
    }

    #[test]
    fn our_own_responses_are_not_treated_as_queries() {
        let r = Responder {
            sock: bind_udp(0).expect("socket"),
            instance_fqdn: format!("deck.{SERVICE}"),
            host_fqdn: "deck.local".into(),
            port: 3240,
            addrs: vec![],
            txt: vec![],
        };
        assert_eq!(r.match_questions(&r.answer(true, None)), None);
    }

    #[test]
    fn txt_strings_round_trip() {
        let e = vec!["a=1".to_string(), "b=two".to_string()];
        assert_eq!(parse_txt(&txt_rdata(&e)), e);
        assert!(parse_txt(&txt_rdata(&[])).is_empty());
    }
}
