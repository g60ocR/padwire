#!/usr/bin/env bash
# Re-verify on the real host.
#
# Every host-side fact in the design notes was measured inside a VM. The
# hypervisor beneath it is the machine that actually runs Sunshine and Steam,
# and it is the importer. Run this there before doing anything else.
#
# Read-only: it claims nothing and attaches nothing.

set -uo pipefail

ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; FAILED=$((FAILED+1)); }
note() { printf '  \033[33m·\033[0m %s\n' "$*"; }
head_() { printf '\n\033[1m%s\033[0m\n' "$*"; }

FAILED=0

head_ "Kernel"
printf '  %s\n' "$(uname -r)"

head_ "vhci-hcd (needed here, on the importer)"
if [ -d /sys/devices/platform/vhci_hcd.0 ]; then
    ok "loaded; $(grep -c . /sys/devices/platform/vhci_hcd.0/status 2>/dev/null || echo '?') status lines"
elif find "/lib/modules/$(uname -r)" -name 'vhci-hcd*' 2>/dev/null | grep -q .; then
    note "present but not loaded — run: sudo modprobe vhci-hcd"
else
    bad "module not found. Fedora: sudo dnf install kernel-modules-extra"
fi

head_ "usbip userspace tools"
if command -v usbip >/dev/null; then
    ok "$(command -v usbip)"
else
    bad "not installed. Fedora: sudo dnf install usbip"
fi

head_ "hid-steam coverage"
if modinfo hid-steam >/dev/null 2>&1; then
    aliases=$(modinfo hid-steam 2>/dev/null | awk '/^alias:/ {print $2}')
    printf '%s\n' "$aliases" | sed 's/^/    /'
    for pid in 1304 1305; do
        if printf '%s' "$aliases" | grep -qi "p0000${pid}"; then
            ok "knows 28de:${pid}"
        else
            note "does not know 28de:${pid}"
        fi
    done
    cat <<'TXT'

    Not knowing the Ibex ids does NOT block Steam. Steam drives the controller
    from userspace over hidraw with its own Triton driver, and hid-generic
    provides /dev/hidrawN for any unclaimed HID device. What you lose until the
    kernel catches up: an evdev gamepad for non-Steam apps, kernel-side
    lizard-mode management, and a kernel gyro/accel sensor device. Steam reads
    the sensors over hidraw regardless.
TXT
else
    note "hid-steam is not available; hid-generic will bind instead, which is enough for Steam"
fi

head_ "Tailscale path"
if command -v tailscale >/dev/null; then
    if tailscale status >/dev/null 2>&1; then
        tailscale status 2>/dev/null | sed 's/^/    /' | head -20
        cat <<'TXT'

    A DERP-relayed path will feel terrible: USB/IP costs one round trip per
    URB, so a relay through a distant region multiplies input latency. Confirm
    a direct path with:
        tailscale ping <exporter>
    If it will not go direct, enable UPnP/NAT-PMP on the router or add a subnet
    route. Treat this as a hard prerequisite, not a nice-to-have.
TXT
    else
        note "installed but not connected"
    fi
else
    note "not installed; usbfwd works over any IP path, but then you are responsible for encryption"
fi

head_ "Local Valve devices (only relevant if this machine is also an exporter)"
if command -v lsusb >/dev/null; then
    if lsusb -d 28de: 2>/dev/null | grep -q .; then
        lsusb -d 28de: | sed 's/^/    /'
        note "record the wired controller's product id here; it is not in SDL's usb_ids.h"
    else
        note "none attached"
    fi
else
    note "lsusb not installed"
fi

head_ "Which machine is this?"
cat <<'TXT'
    The importer is whichever machine runs Sunshine and Steam. That is where
    vhci-hcd and usbfwd-attach belong. If this is a VM and the hypervisor runs
    Steam, run this script there instead.
TXT
if systemctl is-active --quiet sunshine 2>/dev/null; then
    ok "sunshine.service is running here"
elif command -v sunshine >/dev/null; then
    note "sunshine is installed here but not running as a service"
else
    note "no sunshine found here"
fi

head_ "Summary"
if [ "$FAILED" -eq 0 ]; then
    ok "nothing blocking"
else
    bad "$FAILED blocking item(s) above"
fi
exit "$FAILED"
