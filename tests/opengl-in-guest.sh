#!/bin/sh
# Unmodified desktop OpenGL, in the guest, on the host's real GL.
#
#   tests/opengl-in-guest.sh [rootfs-dir]
#
# This runs Debian's own `glxinfo` and `glxgears` -- the aarch64 binaries, as
# shipped -- against Debian's own Mesa, inside the guest, with an Xvfb the
# guest also runs. Nothing in the guest knows gex is there. Mesa's Gallium
# virgl driver speaks the vtest protocol over a UNIX socket, gex serves that
# into the host's libvirglrenderer, and the drawing happens on the host.
#
# What makes it worth a test is everything it does *not* need. There is no
# guest driver, no interposed loader, no shim library, no patched Mesa. All
# of fixed-function GL, immediate mode and display lists -- which glxgears
# uses -- is lowered inside the guest's Mesa before a byte reaches gex. The
# check below is for the renderer string, because that is the one line that
# can only be printed if the whole path worked end to end.
#
# It also guards a failure mode that is not gex's and bites anyway: Mesa's
# vtest winsys does not fall back when the socket is absent. It takes the
# ECONNREFUSED, writes its handshake into the dead descriptor, and then
# blocks forever reading a reply. A regression that stops gex serving the
# socket therefore looks like a hang, not an error, which is why the timeout
# below is part of the test rather than a safety net.
#
# The root filesystem is not built by default; it wants Mesa, Xvfb and
# mesa-utils. Skips when it is absent, or when this build has no
# libvirglrenderer.
set -eu

ROOT="${1:-/tmp/gex-x/root}"
GEXC="${GEXC:-target/release/gexc}"
WORK="${TMPDIR:-/tmp}/gex-opengl-test"

if [ ! -x "$ROOT/usr/bin/glxgears.aarch64-linux-gnu" ] \
    || [ ! -x "$ROOT/usr/bin/Xvfb" ]; then
    echo "skip: no graphics rootfs at $ROOT (wants Mesa, Xvfb and mesa-utils)"
    exit 0
fi

mkdir -p "$WORK"
cat > "$ROOT/opengl-test.sh" <<'SCRIPT'
export DISPLAY=:0
export LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu
mkdir -p /tmp/.X11-unix
Xvfb :0 -screen 0 300x300x24 -nolisten tcp &
i=0
while [ ! -e /tmp/.X11-unix/X0 ]; do
  i=$((i + 1))
  [ "$i" -gt 600 ] && { echo "NO X SOCKET"; exit 1; }
  sleep 1
done
# virpipe is the Gallium driver name for virgl over the vtest socket. It is
# the only thing the guest is told, and it is a stock Mesa environment
# variable, not anything gex invented.
export GALLIUM_DRIVER=virpipe
timeout 120 glxinfo.aarch64-linux-gnu -B
# glxgears is the interesting one: it is immediate-mode fixed-function GL
# from 1997, and it works because the guest's Mesa lowers all of that before
# gex sees it.
timeout 60 glxgears.aarch64-linux-gnu
echo "SCRIPT DONE"
SCRIPT

# Long, because the first run translates all of Mesa and LLVM.
timeout 900 "$GEXC" run --sysroot "$ROOT" --uid 0 --argv0 /bin/sh \
    "$ROOT/bin/dash" /opengl-test.sh >"$WORK/out" 2>&1 || true

renderer=$(sed -n 's/^OpenGL renderer string: //p' "$WORK/out" | head -1)
version=$(sed -n 's/^OpenGL core profile version string: //p' "$WORK/out" | head -1)
frames=$(grep -c "frames in" "$WORK/out" || true)

echo "     renderer: ${renderer:-<none>}"
echo "     core:     ${version:-<none>}"
echo "     glxgears reported $frames frame-rate lines"

if [ -z "$renderer" ]; then
    echo "FAIL opengl-in-guest: glxinfo printed no renderer string"
    tail -20 "$WORK/out" | sed 's/^/     /'
    exit 1
fi
case "$renderer" in
    virgl*) ;;
    *)
        echo "FAIL opengl-in-guest: renderer is \"$renderer\", not virgl --"
        echo "     the guest fell back to software instead of reaching the host"
        exit 1
        ;;
esac
# The frame count matters as much as the renderer string. glxgears reported
# the right renderer and drew exactly one frame for a long time, because the
# device dropped the tail of a reply too large for the socket buffer: every
# component said it had succeeded and the program simply stopped. A renderer
# string proves the connection; a second frame proves the loop.
if [ "$frames" -lt 2 ]; then
    echo "FAIL opengl-in-guest: glxgears reported $frames frame-rate lines --"
    echo "     it reached the renderer and then stopped drawing"
    tail -20 "$WORK/out" | sed 's/^/     /'
    exit 1
fi
echo "ok   opengl-in-guest"
