#!/bin/sh
# Build everything the page serves out of public/.
#
#   web/build-assets.sh [--debian]
#
# The runtime is two wasm modules: the shim (the kernel, the loader and the
# runtime translator) and a seed module that translates nothing. The seed is
# what lets the page run a binary it has never seen -- there is no
# ahead-of-time step in a browser, so every block arrives through a dispatch
# miss.
#
# The rootfs images are plain tars. One request each, and the page unpacks
# them into the guest's in-memory filesystem.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PUB="$HERE/public"
WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}"
TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
BUSYBOX_VERSION=1.36.1

mkdir -p "$PUB/gex" "$PUB/rootfs" "$WORK"

# -- the runtime -----------------------------------------------------------

echo "building the shim..."
"$ROOT/shims/browser/build.sh" >/dev/null
cp "$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm" "$PUB/gex/"

echo "building the seed module..."
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p gexc >/dev/null 2>&1
"$ROOT/target/release/gexc" seed -o "$PUB/gex/seed.wasm" --no-shared-memory

# -- busybox rootfs --------------------------------------------------------

BB="$WORK/busybox"
if [ ! -x "$BB" ]; then
    SRC="$WORK/busybox-$BUSYBOX_VERSION"
    if [ ! -d "$SRC" ]; then
        echo "fetching busybox $BUSYBOX_VERSION..."
        curl -fsSL "https://busybox.net/downloads/busybox-$BUSYBOX_VERSION.tar.bz2" \
            -o "$WORK/busybox.tar.bz2"
        tar -C "$WORK" -xf "$WORK/busybox.tar.bz2"
    fi
    echo "building busybox..."
    make -C "$SRC" defconfig >/dev/null
    sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' "$SRC/.config"
    make -C "$SRC" -j"$(nproc)" \
        CROSS_COMPILE="$TOOLCHAIN/bin/aarch64-linux-musl-" >/dev/null 2>&1
    cp "$SRC/busybox" "$BB"
fi

echo "assembling the busybox rootfs..."
R="$WORK/bbroot"
rm -rf "$R"
mkdir -p "$R/bin" "$R/etc" "$R/root" "$R/tmp" "$R/proc" "$R/dev" "$R/usr/bin"
cp "$BB" "$R/bin/busybox"
# The applet links. `busybox --list` is the authority on which names it
# answers to, so the rootfs matches the binary rather than a guess.
for a in $("$ROOT/target/release/gexc" run "$BB" --list 2>/dev/null); do
    case "$a" in
        */*) continue ;;
    esac
    ln -sf busybox "$R/bin/$a"
done
cat > "$R/etc/profile" <<'EOF'
export PS1='\w \$ '
export PATH=/bin:/usr/bin
EOF
cat > "$R/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat > "$R/etc/group" <<'EOF'
root:x:0:
EOF
cat > "$R/root/hello.c" <<'EOF'
#include <stdio.h>
int main(void) { puts("a file that came with the rootfs"); return 0; }
EOF
tar -C "$R" -cf "$PUB/rootfs/busybox.tar" .
echo "  $PUB/rootfs/busybox.tar: $(du -h "$PUB/rootfs/busybox.tar" | cut -f1)"

# -- debian rootfs ---------------------------------------------------------

if [ "${1-}" = "--debian" ]; then
    D="$WORK/debian"
    if [ ! -d "$D" ]; then
        echo "building a minimal debian arm64 rootfs (needs debootstrap and root)..."
        sudo debootstrap --arch=arm64 --variant=minbase --foreign \
            bookworm "$D" http://deb.debian.org/debian
    fi
    echo "packing the debian rootfs..."
    tar -C "$D" -cf "$PUB/rootfs/debian.tar" .
    echo "  $PUB/rootfs/debian.tar: $(du -h "$PUB/rootfs/debian.tar" | cut -f1)"
fi

echo "done"
