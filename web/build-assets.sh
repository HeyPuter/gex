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

# Do the shim and the glue agree, in both directions -- every export the page
# calls, and every import the shim needs, with the right number of arguments.
# Checked here rather than discovered by a visitor: a mismatch either way
# survives the build, survives loading, and surfaces a minute into a demo.
node "$ROOT/shims/browser/js/check-glue.mjs" \
    "$PUB/gex/gex_browser.wasm" "$ROOT/shims/browser/js/gex.mjs"

# The global kernel module: one memory shared by every Worker of every
# process, so the file table and the process table are simply there and a
# syscall stays a direct call. Needs nightly, for the inline assembly that
# emits a `memory.copy` naming two memories -- Rust has no other spelling for
# reaching a guest buffer that is not in this module's own address space.
#
# Skipped rather than failed without nightly, like the shared-memory shim: a
# machine that cannot build it should still get every other demo.
if rustup toolchain list 2>/dev/null | grep -q nightly; then
    echo "building the global kernel module..."
    "$ROOT/shims/kernel/build.sh"
else
    echo "no global kernel module -- needs nightly"
fi

echo "building the seed module..."
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p gexc >/dev/null 2>&1
"$ROOT/target/release/gexc" seed -o "$PUB/gex/seed.wasm" --no-shared-memory
# And one for a shared memory, to pair with the shim built the same way. A
# memory import's `shared` flag is part of its type, so the two cannot be
# mixed: a page picks a pair by whether it is cross-origin isolated, and the
# shared pair is what lets a process's threads each be a Worker.
"$ROOT/target/release/gexc" seed -o "$PUB/gex/seed.shared.wasm"

# -- virglrenderer, for the guest's own Mesa -------------------------------
#
# The other GL demo splits at the API: the guest links against libGLESv2
# marshallers that forward each call to the page. This one does not split at
# all. The guest runs Debian's Mesa, Mesa's `virgl` Gallium driver writes the
# virtio-gpu command stream it would write to a real GPU, and the thing that
# reads it is virglrenderer -- the same C, compiled to wasm, with WebGL 2
# where its GL binding used to be.
#
# Built by `web/build-virgl-wasm.sh`, which wants emscripten and about ten
# minutes. Copied here when it has been, skipped when it has not, because
# every other demo on the page must keep building without it.
VIRGL_BUILD="${GEX_VIRGL_WASM:-${TMPDIR:-/tmp}/gex-virgl-wasm}"
if [ -f "$VIRGL_BUILD/gexvirgl.wasm" ]; then
    echo "copying the virgl renderer..."
    cp "$VIRGL_BUILD/gexvirgl.wasm" "$VIRGL_BUILD/gexvirgl.mjs" "$PUB/gex/"
    echo "  $(du -h "$PUB/gex/gexvirgl.wasm" | cut -f1) of renderer"
else
    echo "no virgl renderer -- run web/build-virgl-wasm.sh to build one"
fi

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

# `web/build-debian.sh` does this without root and without debootstrap: a
# .deb is an `ar` archive with a tar inside, so unpacking the dependency
# closure of `apt` into a directory is a working userspace. The old branch
# here shelled out to `sudo debootstrap`, which is why the asset was never
# built and the demo failed with "the rootfs has no /bin/bash".
#
# Skipped unless asked for, because it fetches about 40 MB of packages.
if [ "${1-}" = "--debian" ]; then
    "$HERE/build-debian.sh" "$PUB/rootfs/debian.tar"
elif [ ! -f "$PUB/rootfs/debian.tar" ]; then
    echo "no debian rootfs -- run web/build-assets.sh --debian to fetch one"
fi

# -- the graphics rootfs, for the Mesa demo --------------------------------
#
# Debian's own glxgears and Xvfb, and the Mesa they link against. Built from
# the same tree `tests/opengl-in-guest.sh` uses, which is not fetched by
# default -- so this is skipped rather than failed when it is absent.
if [ -x "$HERE/build-graphics-rootfs.sh" ]; then
    "$HERE/build-graphics-rootfs.sh" "$PUB/rootfs/graphics.tar"
fi

# -- the gears, and the GL driver it links against -------------------------
#
# An ordinary GLES2 program, cross-compiled and served as an ELF. The page
# gives the guest a canvas and the guest's libGLESv2 sends every call to it,
# so what appears is drawn by the browser's own GPU stack.
#
# The GL headers have no architecture in them, so the host's serve for a
# cross build; they are copied out rather than used in place so the host's
# libc headers are not dragged in with them.
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"
MUSL="$TOOLCHAIN/aarch64-linux-musl/lib/libc.so"
if [ -x "$CC" ] && [ -d /usr/include/GLES2 ]; then
    echo "building the gears and the guest's GL driver..."
    mkdir -p "$PUB/fs" "$WORK/glinc"
    "$ROOT/guest/gl/build.sh" "$WORK/gldriver" >/dev/null
    cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/glinc/" 2>/dev/null || true
    "$CC" -O2 -o "$PUB/fs/gles-gears" "$ROOT/tests/integration/programs/gles-gears.c" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    cp "$WORK/gldriver/libGLESv2.so.2" "$WORK/gldriver/libEGL.so.1" "$PUB/fs/"
    cp "$MUSL" "$PUB/fs/ld-musl-aarch64.so.1"
else
    echo "skipping the gears: no cross compiler or no GLES2 headers"
fi

# The same for the x86-64 guest (`?arch=x86`): served from fs/x86/, with an
# x86_64 musl toolchain beside the aarch64 one (musl.cc's x86_64-linux-musl-cross).
X86_TOOLCHAIN="${GEX_X86_TOOLCHAIN:-$HOME/toolchains/x86_64-linux-musl-cross}"
X86_CC="$X86_TOOLCHAIN/bin/x86_64-linux-musl-gcc"
if [ -x "$X86_CC" ] && [ -d /usr/include/GLES2 ]; then
    echo "building the x86-64 gears and GL driver..."
    mkdir -p "$PUB/fs/x86" "$WORK/glinc"
    GEX_CC="$X86_CC" "$ROOT/guest/gl/build.sh" "$WORK/gldriver-x86" >/dev/null
    cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/glinc/" 2>/dev/null || true
    "$X86_CC" -O2 -o "$PUB/fs/x86/gles-gears" "$ROOT/tests/integration/programs/gles-gears.c" \
        -I "$WORK/glinc" -L "$WORK/gldriver-x86" -lEGL -lGLESv2 -lm
    cp "$WORK/gldriver-x86/libGLESv2.so.2" "$WORK/gldriver-x86/libEGL.so.1" "$PUB/fs/x86/"
    cp "$X86_TOOLCHAIN/x86_64-linux-musl/lib/libc.so" "$PUB/fs/x86/ld-musl-x86_64.so.1"
else
    echo "skipping the x86-64 gears: no x86_64 musl cross compiler"
fi

echo "done"
