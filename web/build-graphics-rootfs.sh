#!/bin/sh
# The root filesystem the browser's glxgears demo runs in.
#
#   web/build-graphics-rootfs.sh [out.tar] [source-rootfs]
#
# Debian's own aarch64 binaries: `glxgears` and `Xvfb` as shipped, against
# Debian's own Mesa. Nothing here is built for the occasion, and nothing is
# patched -- the guest runs a real graphics driver and a real X server, and
# the only thing that is not Debian's is what sits on the far side of the
# vtest socket.
#
# The source tree is the one `tests/opengl-in-guest.sh` uses, which is 342 MB.
# A page cannot ask for that, so this takes the closure a traced run actually
# opens, which is 204 MB, and then removes the two libraries that make up most
# of it.
#
# # LLVM, and the switch that removes it
#
# Debian ships one megadriver: `swrast_dri.so` is every Gallium driver at
# once, and it declares `libLLVM-15.so.1` in DT_NEEDED because *llvmpipe*
# wants it -- 106 MB, plus the 21 MB of libz3 that LLVM in turn wants. That
# is 127 MB of a 208 MB image, and in a browser it is worse than download
# size: a dispatch slot is claimed per 4 KiB of guest text, a browser will
# not grow the table past ten million of them, and llvmpipe is a great deal
# of text.
#
# `GEX_LLVM_STUB=1` replaces it with a stand-in: the 280 symbols the
# megadriver imports from LLVM, each one a `brk #0`. Nothing on the virgl
# path should call them, and anything that does dies at the instruction with
# the guest PC in the trap, which is a far better failure than a quietly
# different picture.
#
# It does not work, and the question is now settled rather than open. Xvfb
# brings up GLX through the swrast DRI entry points and *calls* into llvmpipe,
# not merely maps it: with the renderer listening and `GALLIUM_DRIVER=virpipe`
# exported before the X server, the run still stopped at
# `gex: breakpoint at pc=0x6d435040` five seconds in, before the X socket
# appeared. The switch remains for anyone who wants to try a client-only
# workload, where nothing brings up a software rasteriser -- but it will not
# get an X server up, so it is off.
#
# Which also means the 106 MB is not avoidable this way. `swrast_dri.so`
# names libLLVM in DT_NEEDED, so ld.so maps it at dlopen whichever Gallium
# driver is chosen, and Xvfb writes out 221 MiB every time it is swapped.
# That is what makes the arena split (task #71) the thing to do instead.
set -eu

OUT="${1:-$(cd "$(dirname "$0")" && pwd)/public/rootfs/graphics.tar}"
SRC="${2:-/tmp/gex-x/root}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}/graphics"
TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"

if [ ! -x "$SRC/usr/bin/glxgears.aarch64-linux-gnu" ] || [ ! -x "$SRC/usr/bin/Xvfb" ]; then
    echo "skip: no graphics rootfs at $SRC (wants Mesa, Xvfb and mesa-utils)"
    exit 0
fi
[ -x "$CC" ] || { echo "skip: no cross compiler at $CC"; exit 0; }

rm -rf "$WORK"
mkdir -p "$WORK"
R="$WORK/root"
mkdir -p "$R"

# What to copy. The library list is the closure of a traced run, less LLVM and
# z3; the programs are named because an execve does not look like an open.
LIBS="
libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libgcc_s.so.1
libexpat.so.1 libz.so.1 libzstd.so.1 liblzma.so.5 libbz2.so.1.0 liblz4.so.1
libselinux.so.1 libpcre2-8.so.0 libcap.so.2 libcap-ng.so.0 libaudit.so.1
libtinfo.so.6 libgpg-error.so.0 libgcrypt.so.20 libsystemd.so.0
libGL.so.1 libGLX.so.0 libGLX_mesa.so.0 libGLdispatch.so.0 libglapi.so.0
libdrm.so.2 libdrm_amdgpu.so.1 libdrm_nouveau.so.2 libdrm_radeon.so.1
libelf.so.1 libsensors.so.5 libstdc++.so.6
libX11.so.6 libX11-xcb.so.1 libXau.so.6 libXdmcp.so.6 libXext.so.6
libXfixes.so.3 libXfont2.so.2 libXxf86vm.so.1 libxkbfile.so.1
libxcb.so.1 libxcb-dri2.so.0 libxcb-dri3.so.0 libxcb-glx.so.0
libxcb-present.so.0 libxcb-randr.so.0 libxcb-shm.so.0 libxcb-sync.so.1
libxcb-xfixes.so.0 libxshmfence.so.1
libbsd.so.0 libmd.so.0 libedit.so.2 libffi.so.8 libunwind.so.8
libfreetype.so.6 libfontenc.so.1 libpng16.so.16 libpixman-1.so.0
libbrotlicommon.so.1 libbrotlidec.so.1 libxml2.so.2
libicuuc.so.72 libicudata.so.72
"
PROGS="Xvfb xkbcomp glxgears.aarch64-linux-gnu glxinfo.aarch64-linux-gnu"

mkdir -p "$R/lib/aarch64-linux-gnu" "$R/usr/lib/aarch64-linux-gnu/dri" \
         "$R/usr/bin" "$R/bin" "$R/etc" "$R/tmp/.X11-unix" "$R/var/lib/xkb" \
         "$R/usr/share/X11" "$R/usr/lib/xorg" "$R/proc" "$R/dev" "$R/root"

# -L, so a name that is a symlink becomes the file. ld.so opens libraries by
# the SONAME, which is the link name, so the target's own name is never
# needed and copying both would double 22 MB of megadriver.
for l in $LIBS; do
    for d in "$SRC/usr/lib/aarch64-linux-gnu" "$SRC/lib/aarch64-linux-gnu"; do
        [ -e "$d/$l" ] || continue
        cp -L "$d/$l" "$R/usr/lib/aarch64-linux-gnu/$l"
        break
    done
    [ -e "$R/usr/lib/aarch64-linux-gnu/$l" ] || echo "  missing: $l"
done
cp -L "$SRC/usr/lib/aarch64-linux-gnu/dri/swrast_dri.so" \
      "$R/usr/lib/aarch64-linux-gnu/dri/swrast_dri.so"
cp -L "$SRC/lib/ld-linux-aarch64.so.1" "$R/lib/ld-linux-aarch64.so.1"

# /lib and /usr/lib are the same directory on a Debian this recent, and the
# guest's ld.so looks in both. Copying twice would double 22 MB, so the
# search path is what points at one place.
for p in $PROGS; do
    [ -e "$SRC/usr/bin/$p" ] && cp -L "$SRC/usr/bin/$p" "$R/usr/bin/$p"
done
cp -L "$SRC/bin/dash" "$R/bin/sh"
# The one coreutils program in the image. dash has no `sleep` builtin, and
# without one the wait for the X socket spins through six hundred iterations
# in a millisecond and gives up before Xvfb has opened its listener.
cp -L "$SRC/bin/sleep" "$R/bin/sleep"
# And `timeout`, so a run can be bounded. glxgears has no frame count and no
# way to stop: it draws until it is killed, and a killed process does not
# flush the five-second report that is the only thing it prints.
cp -L "$SRC/usr/bin/timeout" "$R/usr/bin/timeout"

cp -r "$SRC/usr/share/X11/xkb" "$R/usr/share/X11/xkb"
[ -e "$SRC/usr/share/drirc.d" ] && cp -r "$SRC/usr/share/drirc.d" "$R/usr/share/drirc.d"
[ -e "$SRC/usr/lib/xorg/protocol.txt" ] && cp "$SRC/usr/lib/xorg/protocol.txt" "$R/usr/lib/xorg/"


if [ -n "${GEX_LLVM_STUB:-}" ]; then
    # Generated from the megadriver's own import list, so it stays right when
    # the driver comes from a different Debian.
    readelf -W --dyn-syms "$R/usr/lib/aarch64-linux-gnu/dri/swrast_dri.so" |
        awk '$7 == "UND" && $8 != "" {print $8}' | sed 's/@.*//' | sort -u > "$WORK/undef"
    readelf -W --dyn-syms "$SRC/usr/lib/aarch64-linux-gnu/libLLVM-15.so.1" |
        awk '$7 != "UND" && $8 != "" {print $8}' | sed 's/@.*//' | sort -u > "$WORK/llvm"
    comm -12 "$WORK/undef" "$WORK/llvm" > "$WORK/need"
    {
        echo '.text'
        while read -r sym; do
            printf '.globl %s\n.type %s,@function\n%s:\n  brk #0\n' "$sym" "$sym" "$sym"
        done < "$WORK/need"
    } > "$WORK/llvmstub.s"
    # The version, and it is not decoration: the megadriver imports every one
    # of these as `LLVMFoo@LLVM_15`, and against an unversioned definition
    # glibc's ld.so does not warn -- it fails an assertion in dl-lookup.c and
    # kills the process before `main`.
    printf 'LLVM_15 { global: *; };\n' > "$WORK/llvmstub.map"
    "$CC" -shared -nostdlib -o "$R/usr/lib/aarch64-linux-gnu/libLLVM-15.so.1" \
        "$WORK/llvmstub.s" -Wl,-soname,libLLVM-15.so.1 \
        -Wl,--version-script="$WORK/llvmstub.map"
    echo "  libLLVM: a stub of $(wc -l < "$WORK/need") symbols, $(du -h "$R/usr/lib/aarch64-linux-gnu/libLLVM-15.so.1" | cut -f1)"
else
    for l in libLLVM-15.so.1 libz3.so.4; do
        cp -L "$SRC/usr/lib/aarch64-linux-gnu/$l" "$R/usr/lib/aarch64-linux-gnu/$l"
    done
    echo "  libLLVM: the real one, $(du -h "$R/usr/lib/aarch64-linux-gnu/libLLVM-15.so.1" | cut -f1)"
fi

cat > "$R/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat > "$R/etc/group" <<'EOF'
root:x:0:
EOF

# The script the demo runs, kept in the image so what the page does is
# readable from the tar rather than only from the JavaScript.
cat > "$R/gears.sh" <<'EOF'
export DISPLAY=:0
export LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu
# Before the X server, not after it.
#
# virpipe is Mesa's own name for virgl over a vtest socket, and it is the only
# thing this guest is ever told about its GPU. It used to be exported further
# down, just before the client -- which left *Xvfb* to pick a driver for its
# own GLX, and it picked llvmpipe. The heartbeat caught it: at six and ten
# seconds the guest was executing at 0x6501bd00 and 0x668fbbfc, deep in the
# 106 MB of libLLVM, before the X socket had even appeared.
#
# Nothing in this demo wants a software rasteriser. The renderer is on the
# other side of the socket.
export GALLIUM_DRIVER=virpipe
# Every step says so. In a page this is the only channel there is, and a run
# that is merely slow looks exactly like one that is stuck without it -- the
# first browser attempt at this printed nothing for half an hour and the
# question "where is it" had no answer at all.
echo "gex: starting the X server"
Xvfb :0 -screen 0 300x300x24 -nolisten tcp &
i=0
while [ ! -e /tmp/.X11-unix/X0 ]; do
  i=$((i + 1))
  [ "$i" -gt 600 ] && { echo "gex: no X socket after $i tries"; exit 1; }
  sleep 1
done
echo "gex: X server up after $i seconds"
if [ -e /tmp/.virgl_test ]; then
  echo "gex: the renderer is listening"
else
  echo "gex: no /tmp/.virgl_test -- Mesa will connect to nothing and wait"
fi
echo "gex: loading Mesa and drawing"
# Unbounded unless asked otherwise, and whoever started it decides when to
# stop -- a page closes the tab, a harness gives up. glxgears reports its
# frame rate every five seconds, which is the signal either of them wants.
#
# `timeout` used to bound this and was removed: it forks the program and
# waits, and `wait` came back ECHILD in a browser, so the run ended with
# `timeout: error waiting for command: No child processes` and nothing to say
# whether the gears had drawn. That is a real defect with its own task, but it
# is not this demo's, and having it in the way made every run ambiguous.
if [ -n "${GEARS_SECONDS:-}" ] && [ "${GEARS_SECONDS}" != "0" ]; then
  timeout "$GEARS_SECONDS" glxgears.aarch64-linux-gnu
  s=$?
  # 124 is `timeout` saying it stopped the program, which for a run given a
  # budget is how it is *supposed* to end. It used to be `exec`ed and passed
  # straight out, so a demo that drew for ninety seconds at thirty frames a
  # second reported "exit 124" and coloured itself a failure.
  [ "$s" = 124 ] && s=0
  exit "$s"
fi
exec glxgears.aarch64-linux-gnu
EOF
chmod +x "$R/gears.sh"

mkdir -p "$(dirname "$OUT")"
tar -C "$R" -cf "$OUT" .
echo "  $OUT: $(du -h "$OUT" | cut -f1)"
