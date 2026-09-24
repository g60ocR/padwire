#!/usr/bin/env bash
# usbfwd — install the exporter on a Steam Deck.
#
# Run as root, on the Deck:
#
#     sudo ./install-deck.sh --deck-controls
#
# Root is needed for exactly two things — the udev rule and `loginctl
# enable-linger` — and everything else is installed as the ordinary desktop
# user, because that is where the exporter belongs: a user unit in ~/.config
# survives a SteamOS update, and the udev rule grants access through the seat's
# ACL rather than through privilege.
#
# The udev rule is the one piece that would not survive on its own. Since
# SteamOS 3.6 an OS update throws away every /etc change that is not on a
# keep-list (see /usr/lib/rauc/atomic-update-keep.conf), so the installer adds
# the rule to that list with a drop-in in /etc/atomic-update.conf.d/. Linger
# lives in /var, which an update copies across whole.
#
# Expects to find, next to itself:
#     usbfwd-server            the static musl binary
#     99-usbfwd.rules          the udev rule
#     usbfwd-server.service    the user unit to base the installed one on
#
# Re-running is safe: every step overwrites rather than accumulating.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Devices worth forwarding that are not the machine's own controls: the Proteus
# puck, the Nereid receiver, and the 2026 wired controller.
DEFAULT_ALLOW="28de:1304,28de:1305,28de:1302"
DECK_CONTROLS="28de:1205"
DEFAULT_CHORD="L4+R4"

user="${SUDO_USER:-deck}"
allow="$DEFAULT_ALLOW"
chord=""
hold=""
prefetch=""
binds=()
deck_controls=0
start=1
uninstall=0
force=0
dry=0

usage() {
    cat <<'USAGE'
usbfwd-server installer for SteamOS.

USAGE:
    sudo ./install-deck.sh [OPTIONS]

OPTIONS:
    --deck-controls      Also forward the Deck's own controls (28de:1205).
                         Implies a toggle chord, because forwarding them makes
                         the Deck unusable until something gives them back.
    --allow <PATTERNS>   vid:pid allow-list [default: the puck, the receiver
                         and the wired controller]
    --toggle-chord <S>   Chord that suspends and resumes a forward
                         [default with --deck-controls: L4+R4]
    --toggle-hold <MS>   How long it must be held [default: 1000]
    --bind <ADDR>        Passed through to the server; repeatable
                         [default: the tailnet]
    --prefetch           Keep an interrupt IN URB queued; lower input latency
    --user <NAME>        Install for this user [default: $SUDO_USER, else deck]
    --no-start           Install and enable, but do not start it now
    --uninstall          Remove the unit, the binary and the udev rule
    --force              Skip the refusal to forward 28de:1205 with no chord
    --dry-run            Print what would happen and change nothing. Needs no
                         root, so it is the way to check an invocation first.
    -h, --help           This

After installing, confirm the chord against the hardware:

    ~/.local/bin/usbfwd-server --chord-probe

and watch what the service is doing with:

    journalctl --user -u usbfwd-server -f
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --deck-controls) deck_controls=1 ;;
        --allow) allow="${2:?--allow needs a value}"; shift ;;
        --toggle-chord) chord="${2:?--toggle-chord needs a value}"; shift ;;
        --toggle-hold) hold="${2:?--toggle-hold needs a value}"; shift ;;
        --bind) binds+=("${2:?--bind needs a value}"); shift ;;
        --prefetch) prefetch=1 ;;
        --user) user="${2:?--user needs a value}"; shift ;;
        --no-start) start=0 ;;
        --uninstall) uninstall=1 ;;
        --force) force=1 ;;
        --dry-run) dry=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install-deck.sh: unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

die() { echo "install-deck.sh: $*" >&2; exit 1; }

[[ $EUID -eq 0 || $dry -eq 1 ]] || die "run this as root (sudo ./install-deck.sh)"

# Everything that changes the machine goes through here, so --dry-run is a
# property of the script rather than a promise in its documentation.
run() {
    if [[ $dry -eq 1 ]]; then
        echo "  would run: $*"
    else
        "$@"
    fi
}

pw="$(getent passwd "$user")" || die "no such user: $user"
home="$(cut -d: -f6 <<<"$pw")"
uid="$(cut -d: -f3 <<<"$pw")"
group="$(id -gn "$user")"
[[ -d "$home" ]] || die "$user has no home directory at $home"

unit_dir="$home/.config/systemd/user"
unit="$unit_dir/usbfwd-server.service"
bin="$home/.local/bin/usbfwd-server"
rule="/etc/udev/rules.d/99-usbfwd.rules"
keep="/etc/atomic-update.conf.d/usbfwd.conf"

# systemctl --user needs the user's own manager, which needs its runtime dir.
as_user() {
    if [[ "$(id -un)" == "$user" ]]; then
        env XDG_RUNTIME_DIR="/run/user/$uid" "$@"
    else
        runuser -u "$user" -- env XDG_RUNTIME_DIR="/run/user/$uid" "$@"
    fi
}

if [[ $uninstall -eq 1 ]]; then
    echo "Removing usbfwd from $user's account..."
    run as_user systemctl --user disable --now usbfwd-server.service 2>/dev/null || true
    run rm -f "$unit" "$bin" "$rule" "$keep"
    run as_user systemctl --user daemon-reload 2>/dev/null || true
    run udevadm control --reload-rules || true
    echo "Done. Linger is left enabled; turn it off with:"
    echo "    loginctl disable-linger $user"
    exit 0
fi

for f in usbfwd-server 99-usbfwd.rules usbfwd-server.service; do
    [[ -f "$here/$f" ]] || die "$f is missing from $here"
done

# Forwarding the Deck's own controls without a way to take them back leaves a
# handheld whose only input is the touchscreen. Refuse rather than explain it
# afterwards.
if [[ $deck_controls -eq 1 ]]; then
    case ",$allow," in
        *",$DECK_CONTROLS,"*) ;;
        *) allow="$allow,$DECK_CONTROLS" ;;
    esac
    [[ -n "$chord" ]] || chord="$DEFAULT_CHORD"
fi
if [[ -z "$chord" && $force -eq 0 ]]; then
    if [[ "$allow" == *"$DECK_CONTROLS"* || "$allow" == *"28de:*"* || "$allow" == *"*:*"* ]]; then
        die "this allow-list covers the Deck's own controls ($DECK_CONTROLS) but sets no
    --toggle-chord, so nothing would give them back once a host imports them.
    Add --toggle-chord, or --force if you meant it."
    fi
fi

echo "Installing usbfwd-server for $user ($home)"
echo "  allow-list : $allow"
[[ -n "$chord" ]] && echo "  chord      : $chord${hold:+, held ${hold}ms}"

# 1. The udev rule. Root's only real job: it is what lets a user-level service
#    claim the USB device, and read the hidraw nodes the chord needs.
run install -m 0644 "$here/99-usbfwd.rules" "$rule"
run udevadm control --reload-rules
run udevadm trigger --subsystem-match=usb --subsystem-match=hidraw || true

#    An OS update would otherwise delete the rule and leave a service that
#    starts but can open nothing. Only where the atomic updater exists: on any
#    other distro /etc is simply persistent.
if [[ -d /usr/lib/rauc || -d /etc/atomic-update.conf.d ]]; then
    if [[ $dry -eq 1 ]]; then
        echo "  would write $keep with:"
        echo "    $rule"
    else
        install -d -m 0755 /etc/atomic-update.conf.d
        printf '%s\n' "# usbfwd: keep the udev rule across SteamOS updates." "$rule" >"$keep"
        chmod 0644 "$keep"
    fi
fi

# 2. The binary, owned by the user who will run it.
run install -D -m 0755 -o "$user" -g "$group" "$here/usbfwd-server" "$bin"

# 3. The unit, with the arguments this install chose. Built from the packaged
#    one so the hardening settings and the comments stay in one place.
args=()
[[ -n "$allow" ]] && args+=(--allow "$allow")
[[ -n "$chord" ]] && args+=(--toggle-chord "$chord")
[[ -n "$hold" ]] && args+=(--toggle-hold "$hold")
[[ -n "$prefetch" ]] && args+=(--prefetch)
for b in ${binds+"${binds[@]}"}; do args+=(--bind "$b"); done

quoted=""
for a in ${args+"${args[@]}"}; do
    # systemd splits on whitespace, so anything containing it needs quoting.
    [[ "$a" == *[[:space:]]* ]] && a="\"$a\""
    quoted+=" $a"
done

grep -q '^ExecStart=%h/\.local/bin/usbfwd-server$' "$here/usbfwd-server.service" ||
    die "the unit template has no plain ExecStart line to patch"

if [[ $dry -eq 1 ]]; then
    echo "  would write $unit with:"
    echo "    ExecStart=%h/.local/bin/usbfwd-server$quoted"
else
    install -d -m 0755 -o "$user" -g "$group" "$unit_dir"
    sed "s|^ExecStart=%h/.local/bin/usbfwd-server$|ExecStart=%h/.local/bin/usbfwd-server$quoted|" \
        "$here/usbfwd-server.service" >"$unit"
    chown "$user:$group" "$unit"
    chmod 0644 "$unit"
fi

# 4. Linger, so it keeps running when nobody is logged in — and so the user
#    manager exists for the systemctl calls below.
run loginctl enable-linger "$user"
if [[ $dry -eq 1 ]]; then
    echo
    echo "Dry run: nothing was changed. The binary that would be installed:"
    "$here/usbfwd-server" --version
    echo
    echo "Exportable right now:"
    "$here/usbfwd-server" --allow "$allow" --list || true
    exit 0
fi
for _ in $(seq 1 50); do
    [[ -d "/run/user/$uid" ]] && break
    sleep 0.1
done
[[ -d "/run/user/$uid" ]] || die "/run/user/$uid never appeared; is systemd-logind running?"

as_user systemctl --user daemon-reload
if [[ $start -eq 1 ]]; then
    # restart, not `enable --now`: on a re-install the service is already
    # running, and --now would leave the old binary serving.
    as_user systemctl --user enable usbfwd-server.service
    as_user systemctl --user restart usbfwd-server.service
else
    as_user systemctl --user enable usbfwd-server.service
fi

echo
echo "Installed:"
echo "  $bin"
echo "  $unit"
echo "  $rule"
[[ -f "$keep" ]] && echo "  $keep"
echo
"$bin" --version
echo
echo "Exportable right now:"
as_user "$bin" --allow "$allow" --list || true

if [[ $start -eq 1 ]]; then
    echo
    as_user systemctl --user --no-pager --lines=15 status usbfwd-server.service || true
fi

cat <<NEXT

Next:
  1. Confirm the chord against the hardware — it only reads, so it is safe to
     run while Steam has the controller:
         $bin --chord-probe
     Hold the buttons you want; it prints the bits and the --toggle-chord to
     use. If they differ from the default, re-run this installer with
     --toggle-chord '<what it printed>'.
  2. Watch it work:
         journalctl --user -u usbfwd-server -f
  3. On the host, add this Deck to /etc/usbfwd/usbfwd.toml and include
     "$DECK_CONTROLS" in its devices list if you want the Deck's own pad.

If the chord ever suspends a forward and cannot resume it, this puts every
device back on offer without needing the controller:
    systemctl --user reload usbfwd-server
NEXT
