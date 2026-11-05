#!/bin/sh
# GObject on wasm32: a G_DEFINE_TYPE type made with g_object_new (GLib
# calls its class and instance initialisers with two arguments, they take
# one), a signal handler without its user data, and g_list_foreach of a
# one-argument function, against GLib built with GEXCC_LENIENT_CALLS=1
# (lenientcalls.sh is the same without GLib). glib/gobject.c (not a run.sh
# program: run.sh has no GLib) is built with gexcc against GLIB_DIR's
# headers and libraries and run on those libraries, under native gexc
# (GEXC) and in the browser runtime (run-node), each against the host's
# output (the host's own GLib, pkg-config).
#
#   test/fixtures/gexcc/dl/gobject.sh DIR   (gexcc on PATH; GLIB_DIR: include/glib-2.0,
#                                 lib/glib-2.0/include and lib/ with wasm32
#                                 libglib-2.0.so.0, libgobject-2.0.so.0,
#                                 libffi.so.8 and libpcre2-8.so.0, without
#                                 it the test is skipped; GEX_ROOT: a gex
#                                 checkout with page/public/gex built; DIR:
#                                 where gex can see, not /dev/shm)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
[ -n "${GLIB_DIR:-}" ] || { echo "skip gobject: no GLIB_DIR"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
G=${GEXC:-gexc}
W=$(mktemp -d "${1:?DIR}/gobject.XXXXXX")
trap 'rm -rf "$W"' EXIT
LIBS="libc.so libpcre2-8.so.0 libffi.so.8 libglib-2.0.so.0 libgobject-2.0.so.0"
L=/usr/lib/wasm32-linux-musl
mkdir -p "$W/root$L"
SYSLIB=$(dirname "$(command -v gexcc)")/../sysroot/lib
for n in $LIBS; do
    f=$GLIB_DIR/lib/$n; [ $n = libc.so ] && f=$SYSLIB/libc.so
    cp -L "$f" "$W/root$L/$n" || { echo "FAIL gobject: no $f"; exit 1; }
done
cc -O1 -w -o "$W/g.native" "$HERE/glib/gobject.c" $(pkg-config --cflags --libs gobject-2.0) || { echo "FAIL build native"; exit 1; }
GEXCC_SHARED=1 gexcc -O2 -w -o "$W/g" "$HERE/glib/gobject.c" -I"$GLIB_DIR/include/glib-2.0" -I"$GLIB_DIR/lib/glib-2.0/include" \
    "$W/root$L/libgobject-2.0.so.0" "$W/root$L/libglib-2.0.so.0" || { echo "FAIL build"; exit 1; }
want=$("$W/g.native")
fail=0
check() { # name got status
    if [ "$2" = "$want" ] && [ "$3" = 0 ]; then echo "ok   gobject $1"; else
        echo "FAIL gobject $1 (exit $3)"; printf '%s\n' "$2" | tail -8; fail=1; fi
}
got=$(cd "$W" && GEX_LOG=off "$G" run --sysroot "$W/root" ./g 2>&1); s=$?
check native "$(printf '%s\n' "$got" | grep -v '^\[')" $s
args=; for n in $LIBS; do args="$args --file $L/$n=$W/root$L/$n"; done
got=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/hosts/browser/js/run-node.mjs" $args \
    --file "/t/g=$W/g" --cwd /t --argv0 ./g "$SEED" "$W/g" 2>&1); s=$?
check browser "$(printf '%s\n' "$got" | grep -v '^\[')" $s
exit $fail
