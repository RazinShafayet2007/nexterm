#!/usr/bin/env bash
# Build the NexTerm .deb.
#
#   ./packaging/build-deb.sh              # build release binaries, then package
#   ./packaging/build-deb.sh --no-build   # package whatever is in target/release
#
# Two deliberate properties:
#
#   * Dependencies are computed **from the binaries** with dpkg-shlibdeps, not
#     written by hand. A hand-written Depends list drifts the moment the engine
#     changes, and the failure mode is a package that installs and then cannot
#     start. (The one hand-written dependency is `Recommends: gnome-terminal`,
#     which is not a linked library — it is the terminal NexTerm drives.)
#   * No `.desktop` file is shipped. `nexterm handler enable` writes its own
#     entry into the *user's* applications dir at runtime, so registration stays
#     opt-in and reversible per user. Shipping one system-wide would register
#     NexTerm for everyone who installs it, which the security model forbids
#     (docs/security-model.md §5).
#
# Output: target/pkg/nexterm_<version>_<arch>.deb
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

PKG=nexterm
ARCH="$(dpkg --print-architecture)"
VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
[ -n "${VERSION:-}" ] || { echo "error: cannot read version from Cargo.toml" >&2; exit 1; }

SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --no-build) SKIP_BUILD=1 ;;
        *) echo "error: unknown argument: $arg" >&2; exit 1 ;;
    esac
done

BINS=(nexterm nexterm-daemon)

export PKG_CONFIG_PATH="${PKG_CONFIG_PATH:-$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig}"

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "== building release binaries"
    cargo build --release --bins
fi

for b in "${BINS[@]}"; do
    [ -x "target/release/$b" ] || {
        echo "error: target/release/$b is missing — build first (drop --no-build)" >&2
        exit 1
    }
done

PKGDIR="target/pkg/${PKG}_${VERSION}_${ARCH}"
WORK="target/pkg/work"
DEB="target/pkg/${PKG}_${VERSION}_${ARCH}.deb"
rm -rf "$PKGDIR" "$WORK"
mkdir -p "$PKGDIR/DEBIAN" "$PKGDIR/usr/bin" "$PKGDIR/usr/share/doc/$PKG" "$WORK/debian"

echo "== staging binaries (stripped)"
for b in "${BINS[@]}"; do
    install -m 0755 "target/release/$b" "$PKGDIR/usr/bin/$b"
    strip --strip-unneeded "$PKGDIR/usr/bin/$b" 2>/dev/null || true
done

echo "== staging docs"
install -m 0644 README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE "$PKGDIR/usr/share/doc/$PKG/"
cat > "$PKGDIR/usr/share/doc/$PKG/copyright" <<EOF
NexTerm is dual-licensed under MIT or Apache-2.0, at your option.
The full texts are installed alongside this file as LICENSE-MIT and
LICENSE-APACHE, and are also available upstream:
  https://github.com/RazinShafayet2007/nexterm

Packaged from the upstream source tree by packaging/build-deb.sh.
EOF

# Directory and file modes are set explicitly: the builder's umask must not leak
# into the package (a 0775 root-owned directory is a different thing from 0755).
find "$PKGDIR" -type d -exec chmod 0755 {} +
find "$PKGDIR/usr/share" -type f -exec chmod 0644 {} +

echo "== computing Depends from the binaries"
# dpkg-shlibdeps wants a source control file to exist; its Depends value is not
# read (the real one is substituted below, after it is computed).
sed -e "s/@ARCH@/$ARCH/" -e "s/@VERSION@/$VERSION/" -e 's/@DEPENDS@/TBD/' \
    packaging/control.in > "$WORK/debian/control"
# Analyze the staged, stripped binaries — the exact files that ship, in the
# layout the package installs them to (dpkg-shlibdeps warns otherwise, and the
# stripped binary is what a dependency check should be about).
shlib_args=()
for b in "${BINS[@]}"; do shlib_args+=(-e "$REPO/$PKGDIR/usr/bin/$b"); done
DEPENDS=""
if shlib_out="$(cd "$WORK" && dpkg-shlibdeps -O "${shlib_args[@]}" 2>"$REPO/target/pkg/shlibdeps.log")"; then
    DEPENDS="$(printf '%s\n' "$shlib_out" | sed -n 's/^shlibs:Depends=//p')"
fi
if [ -z "$DEPENDS" ]; then
    # Fall back to resolving each linked library to its owning package. Less
    # precise (no version constraints) but never empty, so the package always
    # declares something truthful rather than nothing.
    echo "   dpkg-shlibdeps gave nothing (see target/pkg/shlibdeps.log); falling back to ldd"
    DEPENDS="$(for b in "${BINS[@]}"; do
        objdump -p "target/release/$b" 2>/dev/null | sed -n 's/^ *NEEDED *//p'
    done | sort -u | while read -r so; do dpkg -S "$(readlink -f "$so")" 2>/dev/null | cut -d: -f1; done \
        | sort -u | grep -v "^$PKG$" | paste -sd, -)"
fi
[ -n "$DEPENDS" ] || { echo "error: could not determine dependencies" >&2; exit 1; }
echo "   $DEPENDS"

SIZE_KB="$(du -sk "$PKGDIR/usr" | cut -f1)"
# Now fill in the *computed* Depends and keep only the binary stanza: the source
# stanza in control.in exists so dpkg-shlibdeps has one to read, and dpkg-deb
# rejects it inside the built package.
sed -e "s/@ARCH@/$ARCH/" -e "s/@VERSION@/$VERSION/" -e "s|@DEPENDS@|$DEPENDS|" \
    packaging/control.in > "$WORK/control.final"
awk '/^Package: /{binary=1} binary' "$WORK/control.final" \
    | sed -e "/^Version: /a Installed-Size: $SIZE_KB" > "$PKGDIR/DEBIAN/control"

# dpkg wants md5sums for the files it installs (path relative to /, no ./ prefix).
( cd "$PKGDIR" && find usr -type f -exec md5sum {} + > DEBIAN/md5sums )

echo "== building $DEB"
fakeroot dpkg-deb --build --root-owner-group "$PKGDIR" "$DEB" >/dev/null

echo "== control"
dpkg-deb -I "$DEB" | sed -n '/Package:/,$p' | head -20
echo "== contents"
dpkg-deb -c "$DEB" | awk '{print $1, $3, $6, $7, $8}'
echo
echo "built: $DEB ($(du -h "$DEB" | cut -f1))"
