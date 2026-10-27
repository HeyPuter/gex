#!/bin/sh
# Build everything the page serves out of public/.
#
#   web/build-assets.sh
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

# Export providers' modules (glibc's mem*/str*, libm: gex-cgen providers/),
# in a unit cache's layout under gex/cgen/: the page binds them when a
# translated program maps the library they name, with no units
# (`?providers=0` turns them off).
echo "building the export providers..."
rm -rf "$WORK/provcache"
GEX_CGEN_CACHE="$WORK/provcache" "$ROOT/target/release/gexc" cgen-image --providers 2>/dev/null
mkdir -p "$PUB/gex/cgen/providers"
rm -f "$PUB/gex/cgen/providers/"*.wasm
cp "$WORK/provcache/providers/"*.wasm "$PUB/gex/cgen/providers/"

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

# -- the Debian root filesystems -------------------------------------------
#
# rootfs/debian.tar (the debian demo) and rootfs/graphics.tar (the Mesa and
# compositor demos) come from the gex-debian repository (`make debian`,
# `make graphics`); copy them into web/public/rootfs.
for f in debian.tar graphics.tar; do
    [ -f "$PUB/rootfs/$f" ] || echo "no rootfs/$f -- build it with gex-debian"
done

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
    "$CC" -O2 -o "$PUB/fs/gles-gears" "$ROOT/guest/gl/probes/gles-gears.c" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-uniform-probe" \
        "$ROOT/guest/gl/probes/gl44-uniform-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-state-probe" \
        "$ROOT/guest/gl/probes/gl44-state-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-clear-probe" \
        "$ROOT/guest/gl/probes/gl44-clear-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-msaa-blit-probe" \
        "$ROOT/guest/gl/probes/gl44-msaa-blit-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-msaa-depth-blit-probe" \
        "$ROOT/guest/gl/probes/gl44-msaa-depth-blit-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-indirect-probe" \
        "$ROOT/guest/gl/probes/gl44-indirect-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-reflection-probe" \
        "$ROOT/guest/gl/probes/gl44-reflection-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-indexed-probe" \
        "$ROOT/guest/gl/probes/gl44-indexed-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-compute-probe" \
        "$ROOT/guest/gl/probes/gl44-compute-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl43-msvo-probe" \
        "$ROOT/guest/gl/probes/gl43-msvo-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-persistent-probe" \
        "$ROOT/guest/gl/probes/gl44-persistent-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-texture-sampler-probe" \
        "$ROOT/guest/gl/probes/gl44-texture-sampler-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    "$CC" -O2 -o "$PUB/fs/gl44-integer-texture-probe" \
        "$ROOT/guest/gl/probes/gl44-integer-texture-probe.c" \
        -I "$ROOT/third_party/mesa/include" \
        -I "$WORK/glinc" -L "$WORK/gldriver" -lEGL -lGLESv2 -lm
    cp "$WORK/gldriver/libGLESv2.so.2" "$WORK/gldriver/libEGL.so.1" "$PUB/fs/"
    # The packaged desktop applications are Debian/glibc, while gles-gears is
    # intentionally a tiny musl binary. Build the same marshaller against each
    # ABI instead of making either process load the other C runtime.
    if command -v strat >/dev/null 2>&1 && strat debian test -x /usr/bin/aarch64-linux-gnu-gcc; then
        mkdir -p "$PUB/fs/glibc"
        strat debian env GEX_CC=/usr/bin/aarch64-linux-gnu-gcc \
            "$ROOT/guest/gl/build.sh" "$WORK/gldriver-glibc" >/dev/null
        cp "$WORK/gldriver-glibc/libGLESv2.so.2" \
           "$WORK/gldriver-glibc/libEGL.so.1" \
           "$WORK/gldriver-glibc/libGL.so.1" "$PUB/fs/glibc/"
        # And the GLX probe, which is a guest program rather than a driver but
        # is built by the same compiler and staged the same way.
        GEX_CC=/usr/bin/aarch64-linux-gnu-gcc "$ROOT/guest/glx-init/build.sh" \
            "$WORK/glx-init" >/dev/null 2>&1 &&
            cp "$WORK/glx-init/glx-init" "$WORK/glx-init/egl-init" "$PUB/fs/glibc/" || true
    elif command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
        mkdir -p "$PUB/fs/glibc"
        GEX_CC="$(command -v aarch64-linux-gnu-gcc)" \
            "$ROOT/guest/gl/build.sh" "$WORK/gldriver-glibc" >/dev/null
        cp "$WORK/gldriver-glibc/libGLESv2.so.2" \
           "$WORK/gldriver-glibc/libEGL.so.1" \
           "$WORK/gldriver-glibc/libGL.so.1" "$PUB/fs/glibc/"
        GEX_CC="$(command -v aarch64-linux-gnu-gcc)" "$ROOT/guest/glx-init/build.sh" "$WORK/glx-init" >/dev/null 2>&1 &&
            cp "$WORK/glx-init/glx-init" "$WORK/glx-init/egl-init" "$PUB/fs/glibc/" || true
    elif [ -x "$HOME/toolchains/zig/zig" ]; then
        # zig cc for aarch64 glibc, as web/build-demo.sh does.
        mkdir -p "$PUB/fs/glibc"
        printf '#!/bin/sh\nexec %s cc -target aarch64-linux-gnu.2.36 "$@"\n' "$HOME/toolchains/zig/zig" > "$WORK/aarch64-gnu-cc"
        chmod +x "$WORK/aarch64-gnu-cc"
        GEX_CC="$WORK/aarch64-gnu-cc" "$ROOT/guest/gl/build.sh" "$WORK/gldriver-glibc" >/dev/null
        cp "$WORK/gldriver-glibc/libGLESv2.so.2" \
           "$WORK/gldriver-glibc/libEGL.so.1" \
           "$WORK/gldriver-glibc/libGL.so.1" "$PUB/fs/glibc/"
        GEX_CC="$WORK/aarch64-gnu-cc" "$ROOT/guest/glx-init/build.sh" "$WORK/glx-init" >/dev/null 2>&1 &&
            cp "$WORK/glx-init/glx-init" "$WORK/glx-init/egl-init" "$PUB/fs/glibc/" || true
    else
        echo "skipping glibc GL driver: no aarch64-linux-gnu-gcc or ~/toolchains/zig"
    fi
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
    "$X86_CC" -O2 -o "$PUB/fs/x86/gles-gears" "$ROOT/guest/gl/probes/gles-gears.c" \
        -I "$WORK/glinc" -L "$WORK/gldriver-x86" -lEGL -lGLESv2 -lm
    cp "$WORK/gldriver-x86/libGLESv2.so.2" "$WORK/gldriver-x86/libEGL.so.1" "$PUB/fs/x86/"
    cp "$X86_TOOLCHAIN/x86_64-linux-musl/lib/libc.so" "$PUB/fs/x86/ld-musl-x86_64.so.1"
else
    echo "skipping the x86-64 gears: no x86_64 musl cross compiler"
fi

echo "done"
