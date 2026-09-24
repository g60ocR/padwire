# usbfwd

Forward a Steam Controller — over its Proteus puck or a plain USB cable — from
a Steam Deck or an Android tablet to the machine running Sunshine, so that host
sees a native Steam Controller with full Steam Input while the game streams
back over Moonlight.

It does the one job people otherwise buy VirtualHere for, using USB/IP: an open
protocol whose importing side is already in the Linux kernel. Nothing needs
root on the tablet, nothing needs a kernel module on the Deck, and the host
runs a stock kernel.

## How it works

```
Steam Deck / Android tablet                      Host (Sunshine + Steam)
┌─────────────────────────────────┐              ┌──────────────────────────┐
│ puck 28de:1304  or  wired ctrl  │              │ usbfwd-attach (daemon)   │
│   ↓ usbfs ioctls                │  USB/IP over │   ↓ modprobe + attach    │
│ usbfwd-server                   │──TCP :3240──▶│ vhci-hcd (in-kernel)     │
│   · DISCONNECT_CLAIM            │  Tailscale,  │   ↓                      │
│   · SUBMITURB / REAPURBNDELAY   │  or a local  │ real USB dev, iface 2..5 │
│                                 │   network    │   ↓ hid-generic → hidraw │
└─────────────────────────────────┘              │ Steam Input (full)       │
       no kernel modules needed                  └──────────────────────────┘
       on the exporting side                       stock kernel, no patches
```

* **`usbfwd-server`**, the exporter, runs where the controller is plugged in.
  It takes the device away from the local drivers and serves it over USB/IP.
  It is pure userspace over usbfs, which is what makes it work on SteamOS's
  read-only root and on Android without root. On Android it runs inside the
  app.
* **`usbfwd-attach`**, the importer, runs on the host. It attaches the device
  through the kernel's `vhci-hcd`, which builds a genuine USB device out of
  it, and re-attaches whenever the network drops.

The host sees a real USB device with every interface intact. That matters:
Steam only accepts the puck's controllers on USB interfaces 2–5, which a
virtual HID device cannot provide. [The developer
notes](docs/DEVELOPER-NOTES.md#why-usbip-and-not-a-virtual-hid-device) have
the details.

## Supported devices

| Device | VID:PID |
|---|---|
| Steam Controller, wired (2026) | `28de:1302` |
| Proteus puck (wireless dongle) | `28de:1304` |
| Nereid internal receiver | `28de:1305` |
| A Steam Deck's own controls | `28de:1205`, opt-in — see [below](#forwarding-the-decks-own-controls) |

## Requirements

* **Host:** Linux with `vhci-hcd` (Fedora: `kernel-modules-extra`) and the
  `usbip` tools, running Steam.
* **Exporter:** a Steam Deck, or an Android tablet with USB host support.
* **Network:** a low-latency path between the two: the same local network, or
  the same [Tailscale](https://tailscale.com) tailnet from anywhere. See
  [Network](#network).
* **To build:** Rust with the `x86_64-unknown-linux-musl` target for the Deck;
  the Android SDK, NDK and `cargo-ndk` for the tablet.

## Getting started

### 1. Check the host

On the machine that runs Sunshine and Steam:

```sh
./scripts/host-precheck.sh
```

It is read-only, and reports the kernel, whether `vhci-hcd` and the `usbip`
tools are present, whether `hid-steam` knows the controller's ids, and, if you
use Tailscale, whether it has a direct path.

### 2. Build

```sh
cargo build --release              # the host daemon
./scripts/build-static.sh          # static musl binaries for the Deck
make apk                           # the Android app; see android/README.md
```

### 3. Set up the host

```sh
sudo dnf install usbip kernel-modules-extra     # Fedora
sudo cp target/release/usbfwd-attach /usr/local/bin/
sudo mkdir -p /etc/usbfwd
sudo cp packaging/usbfwd.toml /etc/usbfwd/
sudoedit /etc/usbfwd/usbfwd.toml                # set each exporter's hostname or IP
sudo cp packaging/usbfwd-attach.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now usbfwd-attach
```

Useful without committing to the service:

```sh
usbfwd-attach --list        # what each configured exporter is offering
usbfwd-attach --once        # one attach attempt, then exit
usbfwd-attach --status      # what is attached right now
usbfwd-attach --detach-all
```

### 4a. Set up a Steam Deck

Copy `scripts/install-deck.sh`, the static `usbfwd-server`,
`packaging/99-usbfwd.rules` and `packaging/usbfwd-server.service` into one
directory on the Deck, then:

```sh
./install-deck.sh --dry-run    # what it would do; needs no root
sudo ./install-deck.sh         # do it
```

Without Tailscale, tell it where to listen, e.g. `--bind 192.168.1.20` (see
[Network](#network)).

The exporter runs as a systemd user service, so root is only needed for the
udev rule and `loginctl enable-linger`. Everything the installer writes
survives SteamOS updates. `--uninstall` removes it all, and `--help` lists the
rest (`--deck-controls`, `--prefetch`, `--allow`, …).

Check what it will export, and watch it work:

```sh
~/.local/bin/usbfwd-server --list
journalctl --user -u usbfwd-server -f
```

<details>
<summary>The same steps by hand</summary>

```sh
sudo cp packaging/99-usbfwd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
# keep the rule across SteamOS updates
echo /etc/udev/rules.d/99-usbfwd.rules | sudo tee /etc/atomic-update.conf.d/usbfwd.conf

mkdir -p ~/.local/bin ~/.config/systemd/user
cp usbfwd-server ~/.local/bin/
cp usbfwd-server.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now usbfwd-server
sudo loginctl enable-linger "$USER"
```

</details>

### 4b. Set up an Android tablet

Build and install the APK (see [`android/README.md`](android/README.md)), then
plug the controller in. The app launches on its own and gets USB permission
from the plug-in itself, so that is the whole interaction. Tick
**Low-latency prefetch** for snappier input.

## Options worth knowing

| Option | What it does |
|---|---|
| `--prefetch` | Keeps a request queued on the controller at all times, so input is captured the moment it happens rather than when the host next asks. Noticeably snappier over a real network. Off by default; a checkbox in the Android app. |
| `--allow <vid:pid,…>` | Which devices to export. The server defaults to every Valve device (`28de:*`); the Deck installer narrows that to the controller, puck and receiver. |
| `--toggle-chord <buttons>` | A button chord that hands a forwarded device back and forth. See below. |
| `--bind <addr>` | Where to listen. Defaults to the Tailscale address, waiting for Tailscale to come up; see [Network](#network) for running without it. |
| `--mdns` | Advertise the exporter on the local network, so the host can find it without being told its address. |

## Forwarding the Deck's own controls

The Deck's built-in controls are an ordinary USB device (`28de:1205`), so they
can be forwarded too. That turns the Deck into a controller for the host, but
while it is forwarded the Deck's own UI stops responding. `--toggle-chord`
makes that reversible from the controller itself:

```sh
sudo ./install-deck.sh --deck-controls    # forwards 28de:1205 with an L4+R4 chord
```

Hold the chord (for a second by default) and the Deck takes its controls back;
hold it again and the host gets them back. Add `28de:1205` to the host's
`devices` list and the reattach is automatic.

Before relying on it, confirm the chord against your hardware:

```sh
~/.local/bin/usbfwd-server --chord-probe
```

Hold the buttons you want and it prints the `--toggle-chord` value to use.

If the controls ever get stuck on the host, this gives them back without
needing the controller:

```sh
systemctl --user reload usbfwd-server
```

Things to know:

* **Each toggle is a real USB disconnect and reconnect on the host.** Steam
  re-detects the controller, games may announce it, and the player index can
  move.
* **Pick buttons nothing else uses.** While the controls are back on the Deck,
  the Deck's own Steam sees the chord too.

## Network

USB/IP is **unencrypted**, and it costs a network round trip for every
transfer, so the link matters more than anything in the code. There are two
ways to run it.

### Over Tailscale (the default)

The exporter listens only on its Tailscale address, and waits for Tailscale to
come up if it has not yet. Tailscale supplies the encryption, and its ACLs are
the access control, so this is the safe way to use it outside your home, or
between two sites. On the host, set each exporter's MagicDNS name in
`/etc/usbfwd/usbfwd.toml`.

Make sure the path is **direct** rather than relayed: `tailscale ping
<exporter>` says which. A path relayed through a DERP server is the difference
between "feels native" and "unplayable"; enable UPnP/NAT-PMP on the router, or
a subnet route, if it will not go direct.

### On a local network

Without Tailscale, the exporter has to be told where to listen:

```sh
usbfwd-server --bind 192.168.1.20      # the exporter's own LAN address
usbfwd-server --bind any               # every interface
sudo ./install-deck.sh --bind 192.168.1.20
```

The Android app does this on its own: with no Tailscale on the tablet, it
listens on every interface.

On the host, either put the exporter's hostname or IP in `usbfwd.toml`, or run
the exporter with `--mdns` and set `mdns = true` under `[discovery]` so the
host finds it by itself.

Only do this on a network you trust. Anyone who can reach port 3240 can take
the controller and see its input, and there is deliberately no pairing scheme:
if you need one, use Tailscale or another VPN, which does it better.

### Wi-Fi

* **Turn off Wi-Fi power saving** on the exporter. It batches packets and shows
  up as stutter.
  * **Steam Deck:** SteamOS manages this itself and will undo manual changes.
    Enable Developer Mode (Settings → System), then turn off **Wi-Fi power
    management** under Settings → Developer.
  * **Other Linux exporters:**
    ```sh
    sudo iw dev wlan0 set power_save off
    # make it stick, with NetworkManager:
    printf '[connection]\nwifi.powersave = 2\n' |
        sudo tee /etc/NetworkManager/conf.d/wifi-powersave-off.conf
    ```
* **Prefer 5 GHz**, or a cable for the host, and use `--prefetch`.

## Troubleshooting

Check these in order, because each one predicts the next:

1. **The device reached the host as a USB device.**
   ```sh
   lsusb -t                                        # device on a vhci_hcd bus
   ls /sys/bus/usb/devices/*/bInterfaceNumber      # 1 interface wired, 2–5 for the puck
   ```
   If this is right, Steam will accept the device.
2. **Steam recognises it.** Controller Settings should identify a Steam
   Controller.
3. **`evtest` shows no gyro/accel axes.** That is expected until the kernel's
   `hid-steam` knows the new controller ids. Steam reads the sensors itself.
4. **Reconnect.** Drop the network briefly; the device should come back on its
   own within a few seconds.

Logs:

```sh
journalctl -u usbfwd-attach -f                  # host
journalctl --user -u usbfwd-server -f           # Deck
adb logcat -s usbfwd                            # Android
```

Other things that trip people up:

* **Only one host can import a device at a time.** A second one is refused.
* **Steam on the exporter loses the controller** while it is forwarded. That is
  how it works: the device can only belong to one machine.
* **Only HID-style devices work.** Isochronous transfers (audio, webcams) are
  deliberately unsupported.

## Status

Working end to end: the wired controller and the puck from an Android tablet
over a real tailnet, and the puck through the whole stack on a Linux host. The
Deck exporter runs on SteamOS and reads its own controls.

Not yet verified: Steam's gyro, trackpads and haptics through the forward;
input latency measured rather than inferred; and the chord's button mapping
with someone holding the buttons. See [what has been
verified](docs/DEVELOPER-NOTES.md#what-has-been-verified) for the detail.

## Development

```sh
cargo test --workspace
```

[`docs/DEVELOPER-NOTES.md`](docs/DEVELOPER-NOTES.md) covers the internals: why
USB/IP rather than a virtual HID device, the code layout, how prefetching and
the toggle chord work, how the Deck install survives SteamOS updates, and the
bugs real hardware turned up.

## License

MIT.
