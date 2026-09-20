#!/usr/bin/env bash
# Build fully static binaries against musl.
#
# On SteamOS this is not optional: the root filesystem is immutable and gets
# replaced wholesale by updates, so anything dynamically linked against the
# system libc is one update away from breaking. `libc` is the workspace's only
# dependency, which is what makes this a one-liner rather than a project.

set -euo pipefail

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
cd "$(dirname "$0")/.."

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "Adding the $TARGET target..."
    rustup target add "$TARGET"
fi

cargo build --release --target "$TARGET" -p usbfwd-server -p usbfwd-attach

out="target/$TARGET/release"
echo
for b in usbfwd-server usbfwd-attach; do
    printf '%s\n' "$out/$b"
    file "$out/$b" 2>/dev/null | sed 's/^/    /' || true
    if command -v ldd >/dev/null; then
        ldd "$out/$b" 2>&1 | sed 's/^/    /' || true
    fi
done
