//! End-to-end protocol tests over a real loopback socket.
//!
//! These drive `handle()` — the same function the accept loop calls — so the
//! socket options, the op-code dispatch and the encoders are all the real
//! ones. Nothing here claims a device, so it is safe to run on a machine whose
//! controller is in use.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use usb_backend::{enumerate, DeviceFilter};
use usbfwd_server::registry::Registry;
use usbfwd_server::{handle, never_stop, SysfsDevices};
use usbip_proto::op::{self, OP_REP_DEVLIST, OP_REP_IMPORT, ST_NA, ST_OK};
use usbip_proto::{OpHeader, SIZE_OP_HEADER};

/// Serve exactly one connection with the real handler, then return.
fn serve_one(filter: DeviceFilter) -> (TcpStream, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let h = thread::spawn(move || {
        let (stream, peer) = listener.accept().expect("accept");
        let source = SysfsDevices {
            filter,
            max_transfer: 1 << 20,
        };
        let _ = handle(stream, peer, &source, &Registry::new(), &never_stop(), false);
    });
    let client = TcpStream::connect(addr).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    (client, h)
}

fn exchange(filter: DeviceFilter, request: &[u8]) -> Vec<u8> {
    let (mut c, h) = serve_one(filter);
    c.write_all(request).expect("send request");
    let mut reply = Vec::new();
    c.read_to_end(&mut reply).expect("read reply");
    h.join().unwrap();
    reply
}

#[test]
fn devlist_is_well_formed_and_matches_the_local_filter() {
    let filter: DeviceFilter = "*:*".parse().unwrap();
    let reply = exchange(filter.clone(), &op::encode_devlist_request());
    assert!(reply.len() >= SIZE_OP_HEADER + 4, "reply too short");

    let h = OpHeader::from_bytes(&reply[..SIZE_OP_HEADER]).unwrap();
    h.check_version().expect("version");
    assert_eq!(h.code, OP_REP_DEVLIST);
    assert_eq!(h.status, ST_OK);

    let devs = op::decode_devlist_reply_body(&reply[SIZE_OP_HEADER..]).expect("decode devlist");
    let expected = enumerate::list(&filter).unwrap_or_default();
    assert_eq!(devs.len(), expected.len());

    for (got, want) in devs.iter().zip(expected.iter()) {
        assert_eq!(got.busid, want.busid);
        assert_eq!(got.id_vendor, want.device.id_vendor);
        assert_eq!(got.id_product, want.device.id_product);
        // The interface array length must agree with the count field, or a
        // client would decode the next device from the wrong offset.
        assert_eq!(got.interfaces.len(), got.b_num_interfaces as usize);
        assert_eq!(got.busnum, want.busnum);
        assert_eq!(got.devnum, want.devnum);
    }
}

#[test]
fn an_empty_allow_list_exports_nothing() {
    let reply = exchange("".parse().unwrap(), &op::encode_devlist_request());
    let devs = op::decode_devlist_reply_body(&reply[SIZE_OP_HEADER..]).unwrap();
    assert!(devs.is_empty());
}

#[test]
fn importing_an_unknown_busid_is_refused_without_a_body() {
    let reply = exchange(DeviceFilter::default(), &op::encode_import_request("99-99"));
    assert_eq!(
        reply.len(),
        SIZE_OP_HEADER,
        "a refusal carries no device record"
    );
    let h = OpHeader::from_bytes(&reply).unwrap();
    assert_eq!(h.code, OP_REP_IMPORT);
    assert_eq!(h.status, ST_NA);
}

#[test]
fn a_device_excluded_by_the_filter_cannot_be_imported_by_name() {
    // Naming a real, present device that the allow-list does not cover must
    // still be refused: the bus id arrives over the network.
    let Some(present) = enumerate::list_all().unwrap_or_default().into_iter().next() else {
        return; // no USB devices on this machine
    };
    let reply = exchange(
        "ffff:ffff".parse().unwrap(),
        &op::encode_import_request(&present.busid),
    );
    assert_eq!(reply.len(), SIZE_OP_HEADER);
    assert_eq!(OpHeader::from_bytes(&reply).unwrap().status, ST_NA);
}

#[test]
fn a_traversal_style_busid_is_refused_rather_than_resolved() {
    for evil in ["../../../etc/passwd", "1-2/../../..", "", "1-2\0"] {
        let reply = exchange(DeviceFilter::default(), &op::encode_import_request(evil));
        assert_eq!(
            OpHeader::from_bytes(&reply).unwrap().status,
            ST_NA,
            "{evil:?} must be refused"
        );
    }
}

#[test]
fn a_wrong_protocol_version_is_rejected() {
    let mut req = op::encode_devlist_request();
    req[0] = 0x01;
    req[1] = 0x06; // version 1.0.6
    let (mut c, h) = serve_one(DeviceFilter::default());
    c.write_all(&req).unwrap();
    let mut reply = Vec::new();
    let _ = c.read_to_end(&mut reply);
    h.join().unwrap();
    assert!(reply.is_empty(), "a version mismatch must not be answered");
}
