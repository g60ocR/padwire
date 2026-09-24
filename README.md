# usbfwd

Forward a Steam Controller — over its Proteus puck or a plain USB cable — from
a Steam Deck or an Android tablet to the host running Sunshine, so that host
sees a native Steam Controller with full Steam Input while the game streams
back over Moonlight.

This is what VirtualHere does for $50 per server. VirtualHere is essentially a
polished closed-source USB/IP server, and USB/IP is an open protocol with good
reference implementations, so this is glue and gap-filling rather than a
reimplementation.

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

## Architecture

```
Steam Deck / Android tablet                      Host (Sunshine + Steam)
┌─────────────────────────────────┐              ┌──────────────────────────┐
│ puck 28de:1304  or  wired ctrl  │              │ usbfwd-attach (daemon)   │
│   ↓ usbfs ioctls                │  USB/IP over │   ↓ modprobe + attach    │
│ usbfwd-server                   │──Tailscale──▶│ vhci-hcd (in-kernel)     │
│   · DISCONNECT_CLAIM            │   TCP :3240  │   ↓                      │
│   · SUBMITURB / REAPURBNDELAY   │              │ real USB dev, iface 2..5 │
│   · bound to the tailnet only   │  (WireGuard  │   ↓ hid-generic → hidraw │
└─────────────────────────────────┘   encrypted) │ Steam Input (full)       │
       no kernel modules needed                  └──────────────────────────┘
       on the exporting side                       stock kernel, no patches
```

The load-bearing decision: **the exporting side is pure userspace over usbfs.**
That removes both platform blockers at once — SteamOS's immutable filesystem
and missing `usbip-host` module, and Android's lack of root. Linux and Android
run the same ioctl code and differ only in where the file descriptor comes
from.

## Layout

| Crate | What it is |
|---|---|
| `crates/usbip-proto` | USB/IP 1.1.1 wire format. Encode/decode only, no I/O policy. |
| `crates/usb-backend` | usbfs (`USBDEVFS_*`) device access: descriptors, claiming, the URB queue. |
| `crates/usbfwd-common` | Logging, signals, interface discovery, mDNS. |
| `crates/usbfwd-server` | The exporter: listener, device source, URB pump, chord toggle. |
| `crates/usbfwd-attach` | The host daemon: config, vhci sysfs, reconnect loop. |
| `crates/usbfwd-jni` | `int`-only JNI shim for Android. |
| `android/` | Kotlin app: permission → descriptor → foreground service. |
| `packaging/` | udev rule, systemd units, example config. |
| `scripts/` | Host pre-flight checks, static build. |

`libc` is the only third-party dependency in the whole workspace. That is
deliberate: it makes a fully static musl build a non-event, which matters on
SteamOS where an OS update replaces the root filesystem.

## Getting started

### 0. Check the host first

```sh
./scripts/host-precheck.sh
```

Run it on the machine that actually runs Sunshine and Steam — that machine is
the importer, and it is where `vhci-hcd` and `usbfwd-attach` belong. It reports
the kernel, whether `vhci-hcd` and the `usbip` tools are present, whether
`hid-steam` knows the Ibex ids, and whether Tailscale has a direct path.

> **`hid-steam` not knowing `28de:1304` does not block Steam.** Steam drives the
> controller from userspace over hidraw with its own Triton driver, including
> disabling lizard mode itself (`SDL_hidapi_steam_triton.c:127-144`).
> `hid-generic` binds any unclaimed HID device and provides `/dev/hidrawN`,
> which is all Steam needs. What you give up until the kernel catches up: an
> evdev gamepad for non-Steam applications, kernel-side lizard-mode management,
> and a kernel gyro/accel sensor device — Steam reads the sensors over hidraw
> regardless.

### 1. Build

```sh
cargo build --release              # or:
./scripts/build-static.sh          # static musl binaries for the Deck
```

### 2. On the exporter (Steam Deck)

The short path — copy `scripts/install-deck.sh`, the static `usbfwd-server`,
`packaging/99-usbfwd.rules` and `packaging/usbfwd-server.service` into one
directory on the Deck, then:

```sh
./install-deck.sh --dry-run --deck-controls    # what it would do, no root needed
sudo ./install-deck.sh --deck-controls         # do it
```

Root is needed for exactly two things — the udev rule and `enable-linger` —
and everything else is installed as the desktop user, because that is where the
exporter belongs. It writes the unit with the flags it chose, refuses to
forward `28de:1205` without a `--toggle-chord`, and `--uninstall` undoes all of
it.

Everything survives a SteamOS update. The binary and the unit are in `/home`,
and linger is in `/var`, which an update copies across. The udev rule is the
exception: since SteamOS 3.6 an update discards any `/etc` change that is not on
a keep-list, so the installer adds the rule to one in
`/etc/atomic-update.conf.d/usbfwd.conf`.

The same steps by hand:

```sh
sudo cp packaging/99-usbfwd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
# keep the rule across SteamOS updates
echo /etc/udev/rules.d/99-usbfwd.rules | sudo tee /etc/atomic-update.conf.d/usbfwd.conf

mkdir -p ~/.local/bin ~/.config/systemd/user
cp target/x86_64-unknown-linux-musl/release/usbfwd-server ~/.local/bin/
cp packaging/usbfwd-server.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now usbfwd-server
sudo loginctl enable-linger "$USER"
```

Check what it will export:

```sh
usbfwd-server --list
```

The server binds only to the tailnet by default and waits for Tailscale to come
up rather than failing. Port 3240 carries **unencrypted** USB traffic;
`--bind any` puts it on every network the machine is attached to. Access
control is Tailscale's ACLs — there is deliberately no pairing scheme here,
because the tailnet already supplies encryption and identity.

Two link-layer settings matter more than anything in the code, because USB/IP
costs a round trip per URB:

```sh
# Turn off WiFi power saving — it batches packets and shows up as stutter.
sudo iw dev wlan0 set power_save off
# Make it stick across reboots:
printf '[connection]\nwifi.powersave = 2\n' |
    sudo tee /etc/NetworkManager/conf.d/wifi-powersave-off.conf
```

Prefer 5 GHz, and confirm Tailscale has a **direct** path rather than a DERP
relay (`tailscale ping <host>`). A relayed path through a distant region is the
difference between "feels native" and "unplayable".

### 3. On the host

```sh
sudo dnf install usbip kernel-modules-extra     # Fedora
sudo cp target/release/usbfwd-attach /usr/local/bin/
sudo mkdir -p /etc/usbfwd
sudo cp packaging/usbfwd.toml /etc/usbfwd/
sudoedit /etc/usbfwd/usbfwd.toml                # set the MagicDNS name
sudo cp packaging/usbfwd-attach.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now usbfwd-attach
```

Useful without committing to the service:

```sh
usbfwd-attach --list        # what each configured server is offering
usbfwd-attach --once        # one attach attempt, then exit
usbfwd-attach --status      # what is attached to vhci right now
usbfwd-attach --detach-all
```

### 4. On the tablet

See [`android/README.md`](android/README.md). Short version: build the native
library with `cargo ndk`, install the APK, plug the controller in. The
`USB_DEVICE_ATTACHED` intent filter launches the app and grants permission
implicitly, so plugging in is the whole interaction.

## Forwarding the Deck's own controls

The Deck's built-in controls are an ordinary USB device (`28de:1205`) on an
internal hub, so forwarding them needs no new code at all — but it costs you
the machine you are holding: the exporter evicts the kernel's driver, and the
Deck's UI stops responding until the session ends. `--toggle-chord` makes that
reversible from the controller itself.

```sh
usbfwd-server --chord-probe                       # read the chord off the hardware
usbfwd-server --allow 28de:1205 --toggle-chord L4+R4
```

Hold the chord while it is forwarded and the Deck takes its controls back; hold
it again and the host gets them back. Add `28de:1205` to the host's `devices`
list and the reattach is automatic.

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
holding the Deck keeps their controls, which is the point of the feature. The
lever that needs no controller is `SIGHUP`:

```sh
systemctl --user reload usbfwd-server    # or: kill -HUP $(pidof usbfwd-server)
```

Known costs, none of them hidden:

* **Each toggle is a real USB disconnect and reconnect on the host.** Steam
  re-detects the controller, games may announce it, and the player index can
  move. USB/IP forwards whole devices, so there is no way to forward part of
  the pad and keep the rest local.
* **Pick buttons nothing else uses.** While the device is *not* forwarded the
  Deck's own Steam sees the chord too, and nothing here can stop it.
* **The button-name table is the one unverified part.** `chord::BUTTONS`
  follows SDL's Deck layout, and no Deck was available to confirm it against.
  If a name does not fire, `--chord-probe` prints the bits the device actually
  sends and `--toggle-chord b41+b42` or `--toggle-chord 0x60000000000` takes
  them directly.

## Verifying it works

In order, because each step predicts the next:

1. **The USB topology survived.**
   ```sh
   lsusb -t                                        # device on a vhci_hcd bus
   ls /sys/bus/usb/devices/*/bInterfaceNumber      # 1 iface wired, 2–5 for the puck
   ```
   This single check predicts whether Steam will accept the device.
2. **Steam recognises it.** Controller Settings should identify a Steam
   Controller; check trackpad-as-mouse, gyro and haptics.
3. **`evtest` shows no gyro/accel axes.** Expected until the kernel knows the
   Ibex ids — those come from `hid-steam`. Not a forwarding failure.
4. **End to end.** Play a game on the host through Moonlight, wired first, then
   via the puck.
5. **Kill the network.** Reconnect should be automatic and take a few seconds.
6. **Latency.** Compare HID report intervals locally against forwarded, and
   note whether Tailscale is direct or relayed.

## What has actually been verified

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

Two bugs surfaced only because this test ran against real hardware, and both
are fixed:

1. **Releasing and rebinding had to be split into two passes.** `cdc_acm` binds
   the Communications interface and claims the CDC Data interface during its
   own probe, so rebinding interface 0 while interface 1 was still held by
   usbfs failed with `-EBUSY` and left the puck with no driver on either.
2. **`vhci` reports a fresh port as `VDEV_ST_NOTASSIGNED` for ~300 ms** before
   enumeration promotes it to `VDEV_ST_USED`. Checking for `USED` straight
   after `usbip attach` concluded that nothing had happened.

A first run against the Android exporter — a wired `28de:1302` on a tablet, a
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

A third bug lived in the Android UI rather than the protocol, and cost more
debugging time than either:

5. **The Start button said "Start" after starting.** `startForegroundService`
   is asynchronous, so the `runningPort()` that `refresh()` reads straight
   afterwards is still 0. Tapping again — the natural response to a button
   that appears not to have worked — stopped what the first tap had started,
   which looked from the host exactly like a session dying on its own. The
   button now shows the transition and ignores taps until the port appears.

With those in, **input works end to end over a real tailnet**: the wired
`28de:1302` on an Android tablet, imported by a host in another city, sustains
~34 controller reports per second while the sticks are moving (~3.8 kB/s on
the wire, against 48 bytes every three seconds when the toggles were out of
step), `urbnum` climbing ~38/s, and Steam holding its `hidraw` node throughout.

Not yet verified: Steam exposing gyro, trackpads and haptics through the
forward, and input latency measured rather than inferred from round-trip time.

The toggle has been exercised everywhere it does not need a Deck: the wire
behaviour (a suspended device leaves `OP_REP_DEVLIST` and is refused by name)
runs against this machine's real bus in the protocol tests, and `hidraw`
discovery finds the puck's five nodes.

On a real Deck (SteamOS, kernel 6.16), the static musl binary runs, lists its
own controls as `28de:1205` with five interfaces, and `--chord-probe` finds
their three `hidraw` nodes and **decodes their state reports** — so the report
header and the offset of the 64-bit button field are right on the hardware.
What is still open is the bit *positions*, which need somebody holding the
buttons, and how the handover feels in practice.

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
4. **A DERP-relayed Tailscale path will feel terrible.** USB/IP costs one round
   trip per URB, so a relay through a distant region multiplies input latency.
   Confirm `direct` with `tailscale ping <deck>`; treat it as a hard
   prerequisite and enable UPnP/NAT-PMP or a subnet route if it will not.
5. **Only one host may import a device at a time.** The server enforces this
   and answers a second importer with `ST_NA`.
6. **Tailscale's 1280-byte MTU plus USB/IP's chattiness** means large
   descriptor reads fragment. Harmless, but do not be surprised in a capture.
7. **Forwarding the Deck's own controls** (`28de:1205`) makes the Deck's UI
   uncontrollable while active, which is what `--toggle-chord` exists for — see
   [Forwarding the Deck's own controls](#forwarding-the-decks-own-controls).
   The shipped config still lists the puck and receiver ids explicitly rather
   than `28de:*`, so nothing starts forwarding a handheld's own pad by
   accident.

## Scope

Isochronous transfers are rejected with `-EOPNOTSUPP`, deliberately. They are
the expensive half of a general USB/IP server and no HID device has an
isochronous endpoint. Same for high-bandwidth streaming: a gamepad needs
neither, which is why this is a weekend of work rather than a project.

## Testing

```sh
cargo test --workspace
```

The suite covers the wire format against the layouts in
`Documentation/usb/usbip_protocol.rst` and the in-tree drivers, the usbfs ioctl
encodings and struct offsets against `<linux/usbdevice_fs.h>`, descriptor
parsing against a synthesised 7-interface puck, the chord decoder and its
hold timing against synthesised state reports, and the full handshake over a
loopback socket — including a suspended device disappearing from it. Tests that need real hardware skip themselves when it is
absent; `enumerate` and `vhci` tests run against the live machine when they can.

Recorded device ids:

| Device | VID:PID | Interfaces |
|---|---|---|
| Proteus puck | `28de:1304` | 7 — 0–1 internal comms, 2–5 controller slots, 6 pogo pins |
| Nereid internal receiver | `28de:1305` | as above |
| 2026 controller, wired | `28de:1302` | 1 unified — interrupt IN `0x81` and OUT `0x01`, 64 bytes, 1 ms |
