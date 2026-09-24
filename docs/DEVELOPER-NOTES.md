# Developer notes

padwire's working name was *usbfwd*, which is why the crates, binaries,
services and Android package all still use it.

The internals behind [the README](../README.md): why the design is what it is,
how the less obvious parts work, the bugs that shaped it, and what has and has
not been verified on real hardware.

## Contents

* [Why USB/IP and not a virtual HID device](#why-usbip-and-not-a-virtual-hid-device)
* [Code layout](#code-layout)
* [Gotchas](#gotchas)
* [Interrupt IN prefetching](#interrupt-in-prefetching)
* [How the toggle chord works](#how-the-toggle-chord-works)
* [Surviving SteamOS updates](#surviving-steamos-updates)
* [What has been verified](#what-has-been-verified)
* [Bugs found on real hardware](#bugs-found-on-real-hardware)
* [Testing](#testing)
* [Recorded device ids](#recorded-device-ids)

## Why USB/IP and not a virtual HID device

A `uhid` device is the tempting shortcut and it does not work for the puck.
Valve's own detection gates wireless controllers on USB interface number:

* `SDL_hidapi_steam_triton.c:442-449` accepts Proteus/Nereid dongles only when
  `interface_number >= 2 && interface_number <= 5`.
* `hidapi/linux/hid.c:719,756-759` derives that number by walking sysfs for a
  `usb_interface` parent and reading `bInterfaceNumber`, defaulting to `-1`
  when there is no USB parent.
* A `uhid` device has no USB parent, so it gets `-1`, so it is rejected.
* `hid-steam.c:1706-1732` makes the dependency explicit: it reparents its
  shadow hidraw device onto the real USB interface, commented *"we use the same
  device info than the real interface to trick userspace"*.

`vhci-hcd` builds a genuine USB device out of forwarded descriptors, so
interfaces 2–5 really exist and Steam cannot tell the difference. Gyro,
trackpads, haptics and lizard-mode toggling are all feature reports and
interrupt transfers on those interfaces, carried transparently.

A cable straight into the controller hits a *more permissive* path — the wired
2026 controller presents one unified interface (`hid-steam.c:1606`) and the
`SDL_IsJoystickSteamTriton` branch applies no interface-number check at all —
so wired is strictly the simpler case and the right thing to test first. One
implementation covers both, because USB/IP forwards whole devices.

**`hid-steam` not knowing `28de:1304` does not block Steam.** Steam drives the
controller from userspace over hidraw with its own Triton driver, including
disabling lizard mode itself (`SDL_hidapi_steam_triton.c:127-144`).
`hid-generic` binds any unclaimed HID device and provides `/dev/hidrawN`,
which is all Steam needs. What you give up until the kernel catches up: an
evdev gamepad for non-Steam applications, kernel-side lizard-mode management,
and a kernel gyro/accel sensor device — Steam reads the sensors over hidraw
regardless.

## Code layout

| Crate | What it is |
|---|---|
| `crates/usbip-proto` | USB/IP 1.1.1 wire format. Encode/decode only, no I/O policy. |
| `crates/usb-backend` | usbfs (`USBDEVFS_*`) device access: descriptors, claiming, the URB queue. |
| `crates/usbfwd-common` | Logging, signals, interface discovery, mDNS. |
| `crates/usbfwd-server` | The exporter: listener, device source, URB pump, prefetch, chord toggle. |
| `crates/usbfwd-attach` | The host daemon: config, vhci sysfs, reconnect loop. |
| `crates/usbfwd-jni` | `int`-only JNI shim for Android. |
| `android/` | Kotlin app: permission → descriptor → foreground service. |
| `packaging/` | udev rule, systemd units, example config. |
| `scripts/` | Host pre-flight checks, static build, Deck installer. |

`libc` is the only third-party dependency in the whole workspace. That is
deliberate: it makes a fully static musl build a non-event, which matters on
SteamOS where an OS update replaces the root filesystem.

## Gotchas

1. **Steam on the exporting side fights you for the controller.** The exporter
   evicts it with `USBDEVFS_DISCONNECT_CLAIM`, the userspace equivalent of the
   stub-bind trick the community
   [Steam Controller 2 USB/IP gist](https://gist.github.com/kmobs/b0ccedd0340b34beafa9033f75a0d7c3)
   uses.
2. **`TCP_NODELAY` on both ends.** Non-negotiable — without it Nagle adds tens
   of milliseconds and the controller feels broken. Both ends set it here.
3. **USB/IP drops the whole device on a network stall** rather than recovering.
   That is why `usbfwd-attach` exists; most of its code is the reconnect path.
4. **USB/IP costs one round trip per URB**, so a DERP-relayed Tailscale path
   through a distant region multiplies input latency.
5. **Only one host may import a device at a time.** The server enforces this
   and answers a second importer with `ST_NA`.
6. **Tailscale's 1280-byte MTU plus USB/IP's chattiness** means large
   descriptor reads fragment. Harmless, but do not be surprised in a capture.
7. **The shipped config lists the puck and receiver ids explicitly** rather
   than `28de:*`, so nothing starts forwarding a handheld's own pad
   (`28de:1205`) by accident.
8. **Isochronous transfers are rejected with `-EOPNOTSUPP`**, deliberately.
   They are the expensive half of a general USB/IP server and no HID device has
   an isochronous endpoint. Same for high-bandwidth streaming: a gamepad needs
   neither.

## Interrupt IN prefetching

`--prefetch` (a checkbox in the Android app). Without it the exporter only
submits a URB when a `USBIP_CMD_SUBMIT` arrives, so for a whole round trip no
URB is queued on the controller at all, and every report it produces in that
window is dropped by the exporting kernel.

With it, `prefetch.rs` keeps an internal URB queued on every interrupt IN
endpoint and buffers up to four completed reports. A client submit is answered
from the buffer straight away, or registered and answered by the next report to
land. The rate is still one report per round trip — `usbhid` keeps exactly one
URB in flight and only resubmits on completion — but each report is younger
when it arrives.

Details that matter:

* **Internal seqnums start at `0x8000_0000`**, so the reaper can tell an
  internal completion from one the client is waiting on.
* **The buffer is four deep, and on overflow the oldest goes.** Not one: a
  quick tap is a press and a release inside one round trip, and keeping only
  the newest would swallow the press.
* **A URB stays queued even with the buffer full.** An earlier version stopped
  arming at four, and nothing re-armed it afterwards, so a streaming controller
  froze the forward within one round trip. A client submit also re-arms an
  endpoint left idle by a refused submit, for the same reason.
* **It is off by default** because it changes what a submit means: the answer
  was captured before the request arrived. Right for a HID gamepad, wrong for a
  device where each transfer has to be requested to happen.

## How the toggle chord works

Forwarding the Deck's own controls (`28de:1205`) evicts the kernel's driver, so
the Deck's UI stops responding until the session ends. `--toggle-chord` makes
that reversible from the controller itself.

**"Off" means the device is not on offer.** That is the whole design, and it is
why nothing else had to change:

* the session ends, so the interfaces are released and rebound through the
  teardown path a detach already uses;
* the bus id leaves `OP_REP_DEVLIST` and `OP_REQ_IMPORT` is answered `ST_NA`,
  which `usbfwd-attach` already treats as "not plugged in yet" and waits out;
* the chord is read back off `hidraw` — the node the rebound driver has just
  created — and the device goes back on offer, so the host's next poll attaches
  it again.

**Where the chord is noticed.** While a session is live, every input report
passes through the reaper on its way to the socket, so that is the one place
that sees them all whether they were prefetched or submitted by the client.
With no session there are no URBs at all, so the same detector reads the same
reports from `hidraw` instead — non-exclusive, so Steam on the Deck keeps its
own handle throughout.

**The chord does not reach the host.** A report with the chord down is answered
as a zero-length interrupt IN transfer rather than dropped: dropping would
leave a submit unanswered, and `usbhid` only resubmits when the previous URB
completes, so it would stall input rather than hide one report. A zero-length
transfer is ordinary, and the HID core discards it without handing anything to
`hidraw`.

**The way back, when the way back is broken.** Resuming needs read access to
`/dev/hidraw*`; the udev rule in `packaging/` grants it. Without it the chord
can suspend a forward it cannot resume, so a watcher that cannot open a node
leaves the device **suspended** and says so in the log every 30 s — the person
holding the Deck keeps their controls, which is the point of the feature.
`SIGHUP` puts every suspended device back on offer without the controller.

**The button-name table is the least verified part.** `chord::BUTTONS`
follows SDL's Deck layout. If a name does not fire, `--chord-probe` prints the
bits the device actually sends, and `--toggle-chord b41+b42` or
`--toggle-chord 0x60000000000` takes them directly.

## Surviving SteamOS updates

SteamOS is an A/B image. On update, `/var` is copied to the new slot whole and
`/home` is untouched, but since SteamOS 3.6 every `/etc` change is dropped
unless it is on a keep-list (`/usr/lib/rauc/atomic-update-keep.conf`, plus
drop-ins in `/etc/atomic-update.conf.d/`). Dropped files are backed up to
`/etc/previous` and `/var/lib/steamos-atomupd/etc_backup`.

So the exporter lives where updates cannot reach it:

| Piece | Where | Why it survives |
|---|---|---|
| Binary | `~/.local/bin/usbfwd-server` | `/home` |
| User unit | `~/.config/systemd/user/` | `/home` |
| Linger | `/var/lib/systemd/linger/<user>` | `/var` is copied |
| udev rule | `/etc/udev/rules.d/99-usbfwd.rules` | kept by `/etc/atomic-update.conf.d/usbfwd.conf` |

Without the keep-list entry an update would leave a service that starts but
cannot open the controller, and a chord that cannot resume.

## What has been verified

Run on a Fedora 44 workstation (kernel 7.1.10) with a real Proteus puck
(`28de:1304`) attached, by exporting it to `127.0.0.1` and importing it back
through `vhci-hcd` — a genuine round trip through the whole stack, TCP
included.

* **The USB topology survives.** The forwarded device appears on a `vhci_hcd`
  bus with all seven interfaces and `bInterfaceNumber` 0–6, so slots 2–5 — the
  ones `SDL_hidapi_steam_triton.c` gates on — really exist. `cdc_acm` binds
  interfaces 0–1 and `usbhid` binds 2–6, identical to the directly attached
  device. This is the check that predicts whether Steam will accept it.
* **The URB pump works.** Report descriptors read back over the link at their
  true sizes (372 bytes for each controller slot, 54 for the pogo interface),
  which means control transfers, interrupt transfers and the completion path
  all round-trip. A typical session moves ~245 URBs in the first few seconds.
* **Reconnect works.** Killing the exporter mid-session makes `usbfwd-attach`
  notice the port drop and reattach automatically when it comes back.
* **Teardown restores the device.** After a detach, a `SIGTERM`, or a client
  disappearing, every interface is handed back to its original kernel driver.

**Over a real tailnet, from Android:** the wired `28de:1302` on a tablet,
imported by a host in another city, sustains ~34 controller reports per second
while the sticks are moving (~3.8 kB/s on the wire), `urbnum` climbing ~38/s,
and Steam holding its `hidraw` node throughout. The puck has also been
forwarded from the tablet with `--prefetch` on.

**On a real Deck** (SteamOS 3.8, kernel 6.16), the static musl binary runs,
lists its own controls as `28de:1205` with five interfaces, and `--chord-probe`
finds their three `hidraw` nodes and decodes their state reports — so the
report header and the offset of the 64-bit button field are right on the
hardware. The toggle's wire behaviour (a suspended device leaves
`OP_REP_DEVLIST` and is refused by name) runs against a real bus in the
protocol tests.

**Gyro, trackpads and haptics** work through the forward: Steam on the host
exposes and uses all three.

**Not yet verified:** input latency measured rather than inferred from
round-trip time; the chord's button bit positions with someone holding the
buttons; and the udev rule surviving an actual SteamOS update.

## Bugs found on real hardware

Two surfaced only because the loopback test ran against real hardware:

1. **Releasing and rebinding had to be split into two passes.** `cdc_acm` binds
   the Communications interface and claims the CDC Data interface during its
   own probe, so rebinding interface 0 while interface 1 was still held by
   usbfs failed with `-EBUSY` and left the puck with no driver on either.
2. **`vhci` reports a fresh port as `VDEV_ST_NOTASSIGNED` for ~300 ms** before
   enumeration promotes it to `VDEV_ST_USED`. Checking for `USED` straight
   after `usbip attach` concluded that nothing had happened.

The first run against the Android exporter — a wired `28de:1302` on a tablet, a
real tailnet, the host two cities away — surfaced two more that loopback
structurally could not:

3. **State-changing standard requests were forwarded as raw control URBs.**
   The in-tree server intercepts four of them (`tweak_special_requests()` in
   `drivers/usb/usbip/stub_rx.c`) and this one did not, though it had defined
   the ioctls to do it. A raw `SET_CONFIGURATION` reaches the device, which
   dutifully resets every endpoint's data toggle — but the exporting kernel
   never sees it happen and keeps its own toggles where they were. Android's
   `usbhid` is always bound before the app claims the device, so its toggle for
   the interrupt IN endpoint is already advanced, and after the mismatch that
   endpoint never completes another URB: the device's packets are discarded as
   duplicates. Control and OUT traffic keep working throughout, so the device
   enumerates perfectly, Steam registers it, feature reports land — and no
   input ever arrives.

   The four are now intercepted, but note what the fix actually is: usbfs
   refuses `USBDEVFS_SETCONFIGURATION` with `-EBUSY` whenever an interface is
   claimed, and claiming them all is how the exporter takes the device, so
   that ioctl *always* fails here. Reporting the failure back is worse than
   useless — the importing kernel gives up with `can't set config #1, error
   -16` and drops the device before it is ever usable. So these are answered
   with success regardless, exactly as the in-tree tweaks all `return 0` after
   logging. That is also the right answer for the toggles: the bug was never
   that they need resetting, it was that forwarding the request raw reset the
   *device's* toggles while the exporting kernel's stayed put. Performing it
   on neither side leaves both consistent. `CLEAR_FEATURE(ENDPOINT_HALT)` does
   go through `USBDEVFS_CLEAR_HALT`, which works on a claimed interface and
   resets both sides together, and `SET_ADDRESS` is acknowledged and dropped.
4. **A URB left in flight was inherited by the next session.** Seqnums are
   per-session, but on Android one `UsbfsDevice` outlives every session, so the
   interrupt IN URB still pending at teardown was reaped by the *next*
   importer and answered with a seqnum it had never sent. `vhci` rejects that
   and drops the connection, so the re-attach failed with the port never going
   busy. Invisible on Linux, where each session opens its own device.
   `session::run` now cancels and drains what is in flight before it returns.

Both of these need the exporting kernel to have had a driver bound before
usbfwd claimed the device, which is why exporting to `127.0.0.1` from a machine
where the puck was already free never reproduced either.

A fifth bug lived in the Android UI rather than the protocol, and cost more
debugging time than either:

5. **The Start button said "Start" after starting.** `startForegroundService`
   is asynchronous, so the `runningPort()` that `refresh()` reads straight
   afterwards is still 0. Tapping again — the natural response to a button
   that appears not to have worked — stopped what the first tap had started,
   which looked from the host exactly like a session dying on its own. The
   button now shows the transition and ignores taps until the port appears.

## Testing

```sh
cargo test --workspace
```

The suite covers the wire format against the layouts in
`Documentation/usb/usbip_protocol.rst` and the in-tree drivers, the usbfs ioctl
encodings and struct offsets against `<linux/usbdevice_fs.h>`, descriptor
parsing against a synthesised 7-interface puck, prefetch buffering against a
fake device, the chord decoder and its hold timing against synthesised state
reports, and the full handshake over a loopback socket — including a suspended
device disappearing from it. Tests that need real hardware skip themselves
when it is absent; `enumerate` and `vhci` tests run against the live machine
when they can.

## Recorded device ids

| Device | VID:PID | Interfaces |
|---|---|---|
| Proteus puck | `28de:1304` | 7 — 0–1 internal comms, 2–5 controller slots, 6 pogo pins |
| Nereid internal receiver | `28de:1305` | as above |
| 2026 controller, wired | `28de:1302` | 1 unified — interrupt IN `0x81` and OUT `0x01`, 64 bytes, 1 ms |
| Steam Deck built-in controls | `28de:1205` | 5 |
