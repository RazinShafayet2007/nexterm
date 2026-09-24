#!/usr/bin/env bash
# Bootstrap a user-local sysroot with WebKitGTK .pc files + headers/symlinks.
#
# Why: NexTerm's browser engine (wry/WebKitGTK) needs `libwebkit2gtk-4.1-dev`
# at BUILD time, but this environment has no root access. The WebKitGTK
# *runtime* is already installed system-wide, so the built binary runs fine —
# only the compile-time files are staged here, under ~/.local/share.
#
# On machines where you DO have root, prefer:
#   sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev build-essential pkg-config
#
# Usage:
#   ./scripts/sysroot-webkit.sh
# Then build with:
#   export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
#   cargo build
set -euo pipefail

SYSROOT="$HOME/.local/share/nexterm-sysroot"
WORK="$(mktemp -d /tmp/nexterm-apt.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/cache/archives/partial" "$WORK/state/lists/partial"

echo "==> downloading WebKitGTK dev closure (no root needed)…"
apt-get -o Debug::NoLocking=1 \
    -o Dir::Cache="$WORK/cache" \
    -o Dir::State="$WORK/state" \
    -o Dir::State::status=/var/lib/dpkg/status \
    -o Dir::State::lists=/var/lib/apt/lists \
    -o Dir::Cache::archives="$WORK/cache/archives" \
    --download-only install -y libwebkit2gtk-4.1-dev

echo "==> extracting to $SYSROOT …"
rm -rf "$SYSROOT"
mkdir -p "$SYSROOT"
for deb in "$WORK"/cache/archives/*.deb; do
    dpkg-deb -x "$deb" "$SYSROOT"
done

echo "==> rewriting .pc prefixes to sysroot …"
grep -rl "^prefix=/usr$" \
    "$SYSROOT/usr/lib/x86_64-linux-gnu/pkgconfig/" \
    "$SYSROOT/usr/share/pkgconfig/" 2>/dev/null \
    | while read -r pc; do sed -i "s|^prefix=/usr$|prefix=$SYSROOT/usr|" "$pc"; done

echo "==> re-pointing dangling .so symlinks at system runtime libs …"
# The -dev debs ship `libfoo.so -> libfoo.so.0` symlinks whose targets live
# only in /lib (no root to install them). rust-lld refuses dangling links,
# so re-point each one at the real system file (linker records the SONAME,
# which resolves from the system at RUNTIME — no sysroot needed then).
for link in "$SYSROOT"/usr/lib/x86_64-linux-gnu/*.so; do
    [ -L "$link" ] || continue
    [ -e "$link" ] && continue  # already resolves
    target="$(basename "$(readlink "$link")")"
    real="$(ls /lib/x86_64-linux-gnu/"$target" /usr/lib/x86_64-linux-gnu/"$target" 2>/dev/null | head -1)"
    if [ -n "$real" ]; then
        ln -sf "$real" "$link"
        echo "  $link -> $real"
    else
        echo "  WARNING: no system runtime for $link (target $target)" >&2
    fi
done

echo "==> verifying …"
export PKG_CONFIG_PATH="$SYSROOT/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
for mod in webkit2gtk-4.1 javascriptcoregtk-4.1 libsoup-3.0 gtk+-3.0; do
    printf '  %-24s %s\n' "$mod" "$(pkg-config --modversion "$mod")"
done
echo "sysroot ready at $SYSROOT"
