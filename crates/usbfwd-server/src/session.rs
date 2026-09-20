//! The URB pump: one imported device, three threads.
//!
//! * **reader** — this thread. Reads `USBIP_CMD_*` off the socket and turns
//!   submits into usbfs URBs.
//! * **reaper** — polls the usbfs descriptor and turns completions into
//!   `USBIP_RET_SUBMIT`.
//! * **writer** — the single owner of the socket's send direction, so the
//!   other two never interleave a half-written PDU.
//!
//! Splitting reader from reaper is what keeps input latency down: a submit
//! never waits behind a completion or vice versa. One reaper thread is plenty
//! for a HID device — the whole point of the design is that a gamepad needs
//! neither isochronous transfers nor high-bandwidth streaming.

use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use usb_backend::{SubmitError, UrbRequest, UsbBackend, UsbfsDevice};
use usbfwd_common::{debug, error, info, trace, warn};
use usbip_proto::io::{read_pdu, write_ret_submit, write_ret_unlink};
use usbip_proto::pdu::{Body, RetSubmit};

/// How many replies may queue up before the reaper has to wait for the
/// network. Bounded on purpose: an unbounded queue in front of a stalled
/// tailnet would just turn a latency problem into a memory problem.
const OUTBOX_DEPTH: usize = 512;

/// How long the reaper blocks in `poll()` before re-checking the stop flag.
const REAP_POLL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct Stats {
    pub submitted: AtomicU64,
    pub completed: AtomicU64,
    pub unlinked: AtomicU64,
    pub rejected: AtomicU64,
}

impl Stats {
    fn describe(&self) -> String {
        format!(
            "{} submitted, {} completed, {} unlinked, {} rejected",
            self.submitted.load(Ordering::Relaxed),
            self.completed.load(Ordering::Relaxed),
            self.unlinked.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        )
    }
}

/// Run one import to completion. Returns when the client disconnects, the
/// device disappears, or the process is shutting down.
pub fn run(
    stream: TcpStream,
    dev: Arc<UsbfsDevice>,
    shutting_down: &dyn Fn() -> bool,
) -> io::Result<()> {
    let busid = dev.summary().busid.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let max_transfer = dev.max_transfer();

    let (tx, rx) = sync_channel::<Vec<u8>>(OUTBOX_DEPTH);

    let writer = {
        let out = stream.try_clone()?;
        let stop = Arc::clone(&stop);
        let busid = busid.clone();
        thread::Builder::new()
            .name(format!("usbfwd-tx {busid}"))
            .spawn(move || writer_loop(rx, out, &stop, &busid))?
    };

    let reaper = {
        let dev = Arc::clone(&dev);
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let stats = Arc::clone(&stats);
        let sock = stream.try_clone()?;
        let busid = busid.clone();
        thread::Builder::new()
            .name(format!("usbfwd-rx {busid}"))
            .spawn(move || reaper_loop(dev, tx, &stop, &stats, sock, &busid))?
    };

    let result = reader_loop(
        &stream,
        &dev,
        &tx,
        &stop,
        &stats,
        max_transfer,
        shutting_down,
    );

    // Wind down in an order that cannot deadlock: signal, unblock both
    // directions of the socket, then drop the last sender so the writer's
    // channel closes once the reaper has dropped its own.
    stop.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(Shutdown::Both);
    drop(tx);
    let _ = reaper.join();
    let _ = writer.join();

    // Only now that the reaper has stopped: cancel whatever is still in
    // flight, so the next importer starts with an empty pending table. A URB
    // left here would be reaped by that session and answered with a seqnum
    // from this one, which vhci rejects — it drops the connection and the
    // attach fails. Harmless on Linux, where each session opens its own
    // device, but on Android the device outlives every session.
    dev.cancel_pending();

    info!("{busid}: session ended ({})", stats.describe());
    result
}

fn reader_loop(
    stream: &TcpStream,
    dev: &Arc<UsbfsDevice>,
    tx: &SyncSender<Vec<u8>>,
    stop: &AtomicBool,
    stats: &Stats,
    max_transfer: usize,
    shutting_down: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut sock = stream;
    while !stop.load(Ordering::Relaxed) && !shutting_down() {
        let pdu = match read_pdu(&mut sock, max_transfer) {
            Ok(p) => p,
            Err(e) => {
                return match e.kind() {
                    // The ordinary way a session ends: the importer detached.
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => Ok(()),
                    _ if stop.load(Ordering::Relaxed) => Ok(()),
                    _ => Err(e),
                };
            }
        };

        let seqnum = pdu.header.base.seqnum;
        match pdu.header.body {
            Body::CmdSubmit(c) => {
                if c.is_iso() {
                    // No HID device has an isochronous endpoint, and supporting
                    // them is most of the cost of a general USB/IP server.
                    stats.rejected.fetch_add(1, Ordering::Relaxed);
                    if !post(tx, stop, encode_error(seqnum, -libc::EOPNOTSUPP)) {
                        return Ok(());
                    }
                    continue;
                }
                let req = UrbRequest {
                    seqnum,
                    ep: (pdu.header.base.ep & 0x0f) as u8,
                    dir: pdu.header.base.dir(),
                    transfer_flags: c.transfer_flags,
                    setup: c.setup,
                    buffer_length: c.transfer_buffer_length.max(0) as usize,
                    data: pdu.data,
                };
                trace!(
                    "-> submit seq={seqnum} ep={} dir={:?} len={}",
                    req.ep,
                    req.dir,
                    req.buffer_length
                );
                match dev.submit(req) {
                    Ok(()) => {
                        stats.submitted.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(SubmitError::Handled(status)) => {
                        // Done synchronously, so nothing will be reaped for it
                        // and the completion has to be synthesised here.
                        stats.completed.fetch_add(1, Ordering::Relaxed);
                        trace!("<- handled seq={seqnum} status={status}");
                        if !post(tx, stop, encode_error(seqnum, status)) {
                            return Ok(());
                        }
                    }
                    Err(SubmitError::Urb(status)) => {
                        // Recoverable: tell the client this one URB failed and
                        // keep the session alive.
                        stats.rejected.fetch_add(1, Ordering::Relaxed);
                        debug!("seq={seqnum} rejected with errno {}", -status);
                        if !post(tx, stop, encode_error(seqnum, status)) {
                            return Ok(());
                        }
                    }
                    Err(SubmitError::Fatal(e)) => {
                        error!("submit failed fatally: {e}");
                        return Err(e);
                    }
                }
            }
            Body::CmdUnlink(u) => {
                let cancelled = dev.unlink(u.unlink_seqnum)?;
                stats.unlinked.fetch_add(1, Ordering::Relaxed);
                // -ECONNRESET if we caught it in flight, 0 if it had already
                // completed. Either way the client now owns that URB's fate and
                // the backend suppresses its RET_SUBMIT.
                let status = if cancelled { -libc::ECONNRESET } else { 0 };
                debug!("unlink seq={} -> status {status}", u.unlink_seqnum);
                let mut buf = Vec::new();
                write_ret_unlink(&mut buf, seqnum, status)?;
                if !post(tx, stop, buf) {
                    return Ok(());
                }
            }
            _ => unreachable!("read_pdu only yields USBIP_CMD_* messages"),
        }
    }
    Ok(())
}

fn reaper_loop(
    dev: Arc<UsbfsDevice>,
    tx: SyncSender<Vec<u8>>,
    stop: &AtomicBool,
    stats: &Stats,
    sock: TcpStream,
    busid: &str,
) {
    while !stop.load(Ordering::Relaxed) {
        let wake = match dev.wait(REAP_POLL) {
            Ok(w) => w,
            Err(e) => {
                error!("{busid}: waiting on usbfs failed: {e}");
                break;
            }
        };

        if wake.ready {
            loop {
                match dev.reap() {
                    Ok(Some(c)) => {
                        if c.unlinked {
                            // The client was already told this URB was
                            // cancelled; a second completion would confuse it.
                            trace!("<- dropping completion for unlinked seq={}", c.seqnum);
                            continue;
                        }
                        stats.completed.fetch_add(1, Ordering::Relaxed);
                        trace!(
                            "<- complete seq={} status={} len={}",
                            c.seqnum,
                            c.status,
                            c.actual_length
                        );
                        let mut buf = Vec::with_capacity(48 + c.data.len());
                        let ret = RetSubmit {
                            status: c.status,
                            actual_length: c.actual_length as i32,
                            start_frame: 0,
                            number_of_packets: 0,
                            error_count: 0,
                        };
                        if write_ret_submit(&mut buf, c.seqnum, ret, &c.data).is_err() {
                            break;
                        }
                        if !post(&tx, stop, buf) {
                            stop.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error!("{busid}: reaping failed: {e}");
                        stop.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }

        if wake.gone {
            // Drained above; usbfs keeps completions reapable after a
            // disconnect, so nothing in flight is lost silently.
            warn!("{busid}: device disappeared, ending the session");
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    // Unblock the reader, which is otherwise sitting in read().
    let _ = sock.shutdown(Shutdown::Both);
}

fn writer_loop(rx: Receiver<Vec<u8>>, mut out: TcpStream, stop: &AtomicBool, busid: &str) {
    use std::io::Write;
    while let Ok(buf) = rx.recv() {
        if let Err(e) = out.write_all(&buf) {
            if !stop.load(Ordering::Relaxed) {
                debug!("{busid}: write failed: {e}");
            }
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = out.shutdown(Shutdown::Both);
}

fn encode_error(seqnum: u32, status: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(48);
    let _ = write_ret_submit(
        &mut buf,
        seqnum,
        RetSubmit {
            status,
            ..Default::default()
        },
        &[],
    );
    buf
}

/// Hand a reply to the writer, waiting if the queue is full. Returns false
/// once the session is finished, so callers can unwind instead of spinning.
fn post(tx: &SyncSender<Vec<u8>>, stop: &AtomicBool, buf: Vec<u8>) -> bool {
    let mut buf = buf;
    loop {
        match tx.try_send(buf) {
            Ok(()) => return true,
            Err(TrySendError::Full(b)) => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                buf = b;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usbip_proto::io::read_ret_submit;
    use usbip_proto::pdu::Direction;

    #[test]
    fn a_rejected_submit_encodes_as_a_zero_length_completion() {
        let buf = encode_error(9, -libc::EPIPE);
        assert_eq!(buf.len(), 48);
        let (h, data) = read_ret_submit(&mut &buf[..], |_| Some(Direction::In), 4096).unwrap();
        assert_eq!(h.base.seqnum, 9);
        match h.body {
            Body::RetSubmit(r) => {
                assert_eq!(r.status, -libc::EPIPE);
                assert_eq!(r.actual_length, 0);
            }
            _ => panic!("expected RET_SUBMIT"),
        }
        assert!(data.is_empty());
    }

    #[test]
    fn post_gives_up_once_the_receiver_is_gone() {
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        let stop = AtomicBool::new(false);
        assert!(post(&tx, &stop, vec![1]));
        drop(rx);
        assert!(!post(&tx, &stop, vec![2]));
    }

    #[test]
    fn post_stops_waiting_when_the_session_is_told_to_stop() {
        let (tx, _rx) = sync_channel::<Vec<u8>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        assert!(post(&tx, &stop, vec![1])); // fills the queue
        let s = Arc::clone(&stop);
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            s.store(true, Ordering::SeqCst);
        });
        assert!(!post(&tx, &stop, vec![2]), "must not block forever");
        h.join().unwrap();
    }
}
