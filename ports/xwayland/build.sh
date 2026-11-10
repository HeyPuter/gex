#!/bin/sh
# Build Xwayland for the browser: the X server itself, as WebAssembly.
#
#   ports/xwayland/build.sh [outdir]
#
# WHY
#
# gex already runs rootless Xwayland the obvious way -- as an AArch64 ELF in a
# guest process, translated to wasm like any other program -- and it works.
# What it costs is everything the X server does per frame: a client's XPutImage
# lands in `fb`'s software blitter, and every one of those loops is translated
# AArch64 rather than native wasm. The server is also the one process in a
# session that *every* X client waits on, so it is the worst place in the
# system to be slow, and it holds a whole translated Xwayland + Mesa in a tab's
# memory budget while doing it.
#
# The X server does not have to be a guest program, though. It is ordinary C
# with two connections to the outside world: X clients on one side, a Wayland
# compositor on the other. gex has a Wayland compositor (`gex-wayland`, in the
# shim) and a kernel that owns the unix socket X clients connect to. So the
# server can be compiled for the host instead, and both of its connections
# bridged: X clients reach it through the global kernel's unix sockets, and the
# compositor sits in the same Worker with no socket at all.
#
# WHAT THIS BUILDS
#
# `Xwayland.wasm` plus the static libraries it needs, all cross-built with
# emscripten: pixman, libxcvt, libwayland-client, libxkbfile, libXfont2,
# libfontenc, libXau, zlib, and a SHA-1 for the one xserver call that wants
# libmd. The X protocol headers are the host's -- they are architecture-
# independent -- staged into the prefix so the cross build never reads
# /usr/include.
#
# THE FIVE THINGS THAT NEEDED PATCHING
#
#  1. libwayland calls its listeners through libffi, which has no upstream
#     emscripten port. On wasm32 every Wayland argument is a 32-bit word and
#     every listener returns void, so the arity is the only thing libffi was
#     working out at run time: a switch over it replaces the whole dependency.
#  2. libwayland-server's epoll, signalfd and timerfd are not in emscripten's
#     sysroot. Only the client library is built here, and it uses none of them.
#  3. Xwayland's headers name libdrm's `drmDevice` in the structures that
#     describe buffer sharing and display leasing. Every function that touches
#     those is behind WITH_LIBDRM or glamor, and both are off (there is no DRM
#     node in a tab), so a header with the type names in it is enough.
#  4. `linux/input.h` pulls in the kernel's <linux/types.h>. All Xwayland wants
#     is the KEY_*/BTN_* numbering, which is in input-event-codes.h -- the same
#     table wl_keyboard and wl_pointer speak.
#  5. libxcvt and libxkbfile hard-code `shared_library()`, and wasm-ld has no
#     shared libraries.
#
# WHAT IS OFF, AND WHY
#
#   glamor    needs EGL and GBM on a DRM device. Nothing to point it at, so
#             the server renders with `fb` and pixman -- native wasm speed,
#             which is the point of this build.
#   glx       without glamor there is no GL to hand a GLX client anyway.
#   mitshm    SysV shared memory between a guest process and a host module is
#             not a thing gex can offer; clients fall back to XPutImage.
#   dri3/drm  same as glamor.
#   xdmcp,
#   secure-rpc,
#   xselinux  network X and host security models, none of which apply.
#   input_thread  the server's input thread wants pthreads and signals it has
#             no use for here: one thread, one Worker, blocking is fine.
#
# STATUS
#
# The module links and runs: `Xwayland.js -version` prints the version, and the
# server reaches its listening-socket setup before stopping for want of the gex
# transport. What the page loads is built by the link at the end of this file.
set -eu

# GEX_XWL_THREADS=0: the single-thread build's server.
# Without -pthread anywhere, so its memory is a plain one: the compositor runs
# in the same realm there and reads the pixels by reference, not through a
# SharedArrayBuffer. Its own output directory, published as xwayland-st/.
THREADS=${GEX_XWL_THREADS:-1}
if [ "$THREADS" = 0 ]; then
    # And its one wait (gexos.c gexjs_wait) suspends through JSPI instead of
    # blocking: GEX_XWL_JSPI, linked with -sJSPI.
    PT=""; PTQ="'-DGEX_XWL_JSPI', "; DEFAULT_OUT=${TMPDIR:-/tmp}/gex-xwl-st; PUBDIR=xwayland-st
else
    PT="-pthread"; PTQ="'-pthread', "; DEFAULT_OUT=${TMPDIR:-/tmp}/gex-xwl; PUBDIR=xwayland
fi
OUT="${1:-$DEFAULT_OUT}"
PREFIX="$OUT/prefix"
SRC="$OUT/src"
EMSDK="${EMSDK:-/usr/lib/emsdk}"
XWAYLAND_TAG="${XWAYLAND_TAG:-xwayland-24.1.9}"
WAYLAND_TAG="${WAYLAND_TAG:-}"

HERE="$(cd "$(dirname "$0")" && pwd)"
PATH="$HOME/.local/bin:$PATH"
# GEX_XWL_HOST_TOOLS=<prefix>: the host pieces ports/xwayland/host-tools.sh
# builds when the distro lacks them (wayland-scanner, wayland-protocols,
# xorgproto, xorg-macros, X11/xshmfence.h).
HT="${GEX_XWL_HOST_TOOLS:-}"
if [ -n "$HT" ]; then
    PATH="$HT/bin:$PATH"
    PKG_CONFIG_PATH="$HT/lib/x86_64-linux-gnu/pkgconfig:$HT/share/pkgconfig:$HT/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    ACLOCAL_PATH="$HT/share/aclocal:/usr/share/aclocal${ACLOCAL_PATH:+:$ACLOCAL_PATH}"
    export PKG_CONFIG_PATH ACLOCAL_PATH
fi
command -v meson >/dev/null 2>&1 || { echo "need meson (pip install meson ninja)"; exit 1; }
command -v ninja >/dev/null 2>&1 || { echo "need ninja"; exit 1; }
[ -d "$EMSDK/upstream/emscripten" ] || { echo "no emscripten at $EMSDK"; exit 1; }
EMCC="$EMSDK/upstream/emscripten"

mkdir -p "$PREFIX/lib/pkgconfig" "$PREFIX/include" "$SRC"
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
export EM_PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export PATH="$EMCC:$PATH"

say() { printf '\n== %s\n' "$*"; }

# -- the host's X protocol headers, staged ----------------------------------
#
# xproto and its two dozen siblings are header-only: no code, no architecture.
# Their .pc files point into /usr/include, which a cross build must not read
# (it also holds native pixman, wayland and zlib), so the headers are copied
# and the .pc files re-pointed at the copy.
say "staging the X protocol headers"
[ -d "$PREFIX/include/X11" ] || cp -r /usr/include/X11 "$PREFIX/include/"
# And the host tools' newer protocol headers over the distro's.
[ -z "$HT" ] || [ ! -d "$HT/include/X11" ] || cp -r "$HT/include/X11/." "$PREFIX/include/X11/"
mkdir -p "$PREFIX/include/linux"
cp /usr/include/linux/input-event-codes.h "$PREFIX/include/linux/"
for p in xproto randrproto renderproto xextproto inputproto kbproto fontsproto \
         fixesproto damageproto xcmiscproto bigreqsproto presentproto videoproto \
         compositeproto recordproto scrnsaverproto resourceproto dri3proto \
         xineramaproto xf86bigfontproto xf86vidmodeproto xwaylandproto dpmsproto \
         wayland-scanner wayland-protocols; do
    # The host's pkg-config, not the cross prefix this script just pointed
    # PKG_CONFIG_LIBDIR at -- `env -u` puts the default search path back.
    d=$(env -u PKG_CONFIG_LIBDIR pkg-config --variable=pcfiledir "$p" 2>/dev/null || true)
    [ -n "$d" ] && [ -f "$d/$p.pc" ] || { echo "  missing host package: $p"; exit 1; }
    case $p in
    wayland-*)
        # Build tools and data, not headers: keep them as they are.
        cp "$d/$p.pc" "$PREFIX/lib/pkgconfig/$p.pc" ;;
    *)
        sed -e "s|^prefix=.*|prefix=$PREFIX|" \
            -e "s|^includedir=.*|includedir=$PREFIX/include|" \
            "$d/$p.pc" > "$PREFIX/lib/pkgconfig/$p.pc" ;;
    esac
done

# Xlib's headers, with no library behind them. xserver's meson.build and
# libxkbfile both ask for `x11` to get <X11/Xlib.h>; nothing in a wasm Xwayland
# calls Xlib, and one that did should be an undefined symbol rather than a stub.
cat > "$PREFIX/lib/pkgconfig/x11.pc" <<EOF
prefix=$PREFIX
includedir=\${prefix}/include

Name: x11
Description: Xlib headers only -- see ports/xwayland/build.sh
Version: 1.8.13
Cflags: -I\${includedir}
Libs:
EOF

# libdrm, as far as a type name. See note 3 at the top of this file.
cat > "$PREFIX/include/xf86drm.h" <<'EOF'
/* A stand-in for libdrm's header, for a build with no DRM device.
 *
 * Xwayland's headers name `drmDevice` in the structures that describe buffer
 * sharing and display leasing and include this file unconditionally, while
 * every function that touches those fields is behind WITH_LIBDRM or glamor --
 * both off here. So only the type names have to exist. Anything that really
 * calls into libdrm becomes an undefined symbol at link time, which is what we
 * want: a silent stub would be a rendering path that quietly does nothing. */
#ifndef GEX_XF86DRM_STUB_H
#define GEX_XF86DRM_STUB_H
#include <stdint.h>

typedef struct _drmDevice {
    char **nodes;
    int available_nodes;
    int bustype;
} drmDevice, *drmDevicePtr;

#endif
EOF
cat > "$PREFIX/include/xf86drmMode.h" <<'EOF'
/* See xf86drm.h in this prefix: the mode-setting half of the same stand-in. */
#ifndef GEX_XF86DRMMODE_STUB_H
#define GEX_XF86DRMMODE_STUB_H
#include <xf86drm.h>
#endif
EOF

# -- the gex transport ------------------------------------------------------
#
# `gexos.h` is included by the files patch.py redirects, so it has to be on the
# include path of every one of those builds. `gexos.c` becomes one of the
# server's own sources (see patch.py's patch_meson).
cp "$HERE/gexos.h" "$PREFIX/include/"

# -- the meson cross file ---------------------------------------------------
cat > "$OUT/emscripten.ini" <<EOF
[binaries]
c = '$EMCC/emcc'
cpp = '$EMCC/em++'
ar = '$EMCC/emar'
strip = '$EMCC/emstrip'
pkg-config = 'pkg-config'

[built-in options]
c_args = [$PTQ'-I$PREFIX/include', '-Wno-error', '-Wno-pedantic', '-Wno-c23-extensions', '-Wno-unused-command-line-argument']
c_link_args = [$PTQ'-L$PREFIX/lib', '-lz']
prefix = '$PREFIX'
libdir = 'lib'

[properties]
needs_exe_wrapper = true
skip_sanity_check = true

[host_machine]
system = 'emscripten'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
EOF

clone() { # clone <dir> <url> [tag]
    [ -d "$SRC/$1" ] && return 0
    if [ $# -ge 3 ]; then
        git clone --depth 1 --branch "$3" "$2" "$SRC/$1" >/dev/null 2>&1
    else
        git clone --depth 1 "$2" "$SRC/$1" >/dev/null 2>&1
    fi
}

# The single-thread build: meson's dependency('threads') is -pthread on an
# emscripten target whatever the cross file says, and a library's .pc then
# carries it into every link after it. Taken out of the generated build and the
# installed .pc files, so no object is built for, and no link asks for, a
# shared memory.
strip_pthread() { # strip_pthread <build dir>
    [ "$THREADS" = 0 ] || return 0
    sed -i -e 's/ -pthread\b//g' -e 's/ -sPTHREAD_POOL_SIZE=[0-9]*//g' "$1/build.ninja"
    sed -i -e 's/ *-pthread\b//g' -e 's/ *-sPTHREAD_POOL_SIZE=[0-9]*//g' "$PREFIX"/lib/pkgconfig/*.pc 2>/dev/null || true
}

meson_build() { # meson_build <dir> <extra meson args...>
    d=$1; shift
    rm -rf "$OUT/build/$d"
    # `-Dc_thread_count=0` in *every* build, not only the server's: for an
    # emscripten target meson appends -sPTHREAD_POOL_SIZE=<c_thread_count> to a
    # library's link flags, and pixman's generated pixman-1.pc then carries
    # `-pthread -sPTHREAD_POOL_SIZE=4` into every link that depends on it --
    # after the server's own flags, so it wins. Four Workers the server never
    # uses, and it blocks before `main` waiting for them to load.
    meson setup "$OUT/build/$d" "$SRC/$d" --cross-file "$OUT/emscripten.ini" \
        --prefix="$PREFIX" --buildtype=release -Ddefault_library=static \
        -Dc_thread_count=0 "$@" \
        > "$OUT/log-$d.txt" 2>&1
    strip_pthread "$OUT/build/$d"
    ninja -C "$OUT/build/$d" install >> "$OUT/log-$d.txt" 2>&1
    strip_pthread "$OUT/build/$d"
}

auto_build() { # auto_build <dir> <extra configure args...>
    d=$1; shift
    cd "$SRC/$d"
    [ -f configure ] || { NOCONFIGURE=1 ./autogen.sh > "$OUT/log-$d.txt" 2>&1 || \
                          autoreconf -fiv > "$OUT/log-$d.txt" 2>&1; }
    emconfigure ./configure --host=wasm32-unknown-emscripten --prefix="$PREFIX" \
        --enable-static --disable-shared \
        CFLAGS="$PT -O2 -Wno-error -I$PREFIX/include" \
        LDFLAGS="$PT -L$PREFIX/lib" "$@" >> "$OUT/log-$d.txt" 2>&1
    emmake make -j"$(nproc)" install >> "$OUT/log-$d.txt" 2>&1
    cd "$OUT"
    # xtrans and libXau put their .pc in share/pkgconfig; pkg-config here is
    # pointed at one directory on purpose, so bring them together.
    cp -f "$PREFIX/share/pkgconfig/"*.pc "$PREFIX/lib/pkgconfig/" 2>/dev/null || true
}

cd "$OUT"

say "sources"
clone xserver https://gitlab.freedesktop.org/xorg/xserver.git "$XWAYLAND_TAG"
clone wayland https://gitlab.freedesktop.org/wayland/wayland.git
# The scanner that generates the protocol code is the host's, and it refuses to
# work for a tree newer than itself -- so use the release that matches it.
WANT_WL="${WAYLAND_TAG:-$(env -u PKG_CONFIG_LIBDIR pkg-config --modversion wayland-scanner)}"
(cd "$SRC/wayland" && git describe --tags --exact-match >/dev/null 2>&1 || {
    git fetch --depth 1 origin tag "$WANT_WL" >/dev/null 2>&1
    git checkout -f -q "$WANT_WL"
})
cp "$HERE/gexos.c" "$HERE/gexgl.c" "$SRC/xserver/hw/xwayland/"
python3 "$HERE/patch.py" sources "$SRC/wayland" "$SRC/xserver"

say "zlib"
clone zlib https://github.com/madler/zlib.git
(cd "$SRC/zlib" && emconfigure ./configure --prefix="$PREFIX" --static > "$OUT/log-zlib.txt" 2>&1 \
    && emmake make -j"$(nproc)" install >> "$OUT/log-zlib.txt" 2>&1)

say "pixman"
clone pixman https://gitlab.freedesktop.org/pixman/pixman.git
meson_build pixman -Dtests=disabled -Ddemos=disabled -Dgtk=disabled -Dlibpng=disabled -Dopenmp=disabled

say "libxcvt"
clone libxcvt https://gitlab.freedesktop.org/xorg/lib/libxcvt.git
python3 - "$SRC/libxcvt" <<'PATCH'
import sys
p = f'{sys.argv[1]}/lib/meson.build'
s = open(p).read()
if 'static_library' not in s:
    # wasm-ld has no shared libraries, and meson's default_library setting does
    # not reach a target that names its kind.
    s = s.replace("""libxcvt = shared_library('xcvt',
                         libxcvt_sources,
                         include_directories : inc,
                         version: meson.project_version(),
                         darwin_versions : ['1.0.0', '1.0.0' ],
                         install : true)""",
                  """libxcvt = static_library('xcvt',
                         libxcvt_sources,
                         include_directories : inc,
                         install : true)""", 1)
    assert 'static_library' in s, 'libxcvt: the library target did not match'
    open(p, 'w').write(s)
PATCH
meson_build libxcvt

say "libwayland (client only, without libffi)"
python3 - "$SRC/wayland" <<'PATCH'
import re, sys
src = sys.argv[1]

# -- libwayland without libffi ------------------------------------------------
p = f'{src}/src/connection.c'
s = open(p).read()
if 'gex_invoke_words' not in s:
    s = s.replace('#include <ffi.h>', '#ifndef __EMSCRIPTEN__\n#include <ffi.h>\n#endif', 1)

    m = re.search(r'static void\nconvert_arguments_to_ffi\(.*?\n\}\n', s, re.S)
    assert m, 'wayland: convert_arguments_to_ffi did not match'
    s = s[:m.start()] + '#ifndef __EMSCRIPTEN__\n' + m.group(0) + '#endif /* !__EMSCRIPTEN__ */\n' + s[m.end():]

    cases = '\n'.join(
        '\tcase %d: ((void (*)(%s))fn)(%s); return;'
        % (n, ', '.join(['uint32_t'] * n) or 'void', ', '.join(f'w[{i}]' for i in range(n)))
        for n in range(24))
    helper = '''#ifdef __EMSCRIPTEN__
/* libwayland without libffi. See ports/xwayland/build.sh.
 *
 * `wl_closure_invoke` has to build a call whose shape it learns at run time
 * from a message signature, which is what libffi is for -- and libffi has no
 * upstream emscripten port. On wasm32 that call is far simpler than the
 * general case: every Wayland argument type (int, uint, fixed, string, object,
 * new_id, array, fd) is a 32-bit word and a listener returns void, so the
 * arity is the only thing that varies. A switch over it is complete.
 *
 * Each cast must name the exact signature: wasm traps on an indirect call
 * whose type does not match the callee's, rather than passing garbage. */
static void
gex_invoke_words(void (*fn)(void), const uint32_t *w, int n)
{
\tswitch (n) {
%s
\t}
\twl_abort("gex_invoke_words: %%d arguments is more than this calls\\n", n);
}

static void
convert_arguments_to_words(const char *signature, uint32_t flags,
\t\t\t   union wl_argument *args, int count, uint32_t *w)
{
\tconst char *sig_iter = signature;
\tstruct argument_details arg;
\tint i;

\tfor (i = 0; i < count; i++) {
\t\tsig_iter = get_next_argument(sig_iter, &arg);
\t\tswitch (arg.type) {
\t\tcase WL_ARG_INT:    w[i] = (uint32_t) args[i].i; break;
\t\tcase WL_ARG_UINT:   w[i] = args[i].u; break;
\t\tcase WL_ARG_FIXED:  w[i] = (uint32_t) args[i].f; break;
\t\tcase WL_ARG_STRING: w[i] = (uint32_t) (uintptr_t) args[i].s; break;
\t\tcase WL_ARG_OBJECT: w[i] = (uint32_t) (uintptr_t) args[i].o; break;
\t\tcase WL_ARG_NEW_ID:
\t\t\tw[i] = (flags & WL_CLOSURE_INVOKE_CLIENT)
\t\t\t     ? (uint32_t) (uintptr_t) args[i].o : args[i].n;
\t\t\tbreak;
\t\tcase WL_ARG_ARRAY:  w[i] = (uint32_t) (uintptr_t) args[i].a; break;
\t\tcase WL_ARG_FD:     w[i] = (uint32_t) args[i].h; break;
\t\tdefault: wl_abort("unknown type\\n"); break;
\t\t}
\t}
}
#endif /* __EMSCRIPTEN__ */

''' % cases
    anchor = '#ifndef __EMSCRIPTEN__\nstatic void\nconvert_arguments_to_ffi('
    assert anchor in s
    s = s.replace(anchor, helper + anchor, 1)

    m = re.search(r'void\nwl_closure_invoke\(struct wl_closure \*closure, uint32_t flags,\n'
                  r'\s+struct wl_object \*target, uint32_t opcode, void \*data\)\n\{(.*?)\n\}\n',
                  s, re.S)
    assert m, 'wayland: wl_closure_invoke did not match'
    body = m.group(1)
    assert 'ffi_call' in body
    wasm_body = '''
\tint count;
\tuint32_t words[WL_CLOSURE_MAX_ARGS + 2];
\tvoid (* const *implementation)(void);

\tcount = arg_count_for_signature(closure->message->signature);

\twords[0] = (uint32_t) (uintptr_t) data;
\twords[1] = (uint32_t) (uintptr_t) target;
\tconvert_arguments_to_words(closure->message->signature, flags,
\t\t\t\t   closure->args, count, words + 2);

\timplementation = target->implementation;
\tif (!implementation) {
\t\twl_abort("Implementation of resource %d of %s is NULL\\n",
\t\t\t target->id, target->interface->name);
\t}
\tif (!implementation[opcode]) {
\t\twl_abort("listener function for opcode %u of %s is NULL\\n",
\t\t\t opcode, target->interface->name);
\t}
\tgex_invoke_words(implementation[opcode], words, count + 2);

\twl_closure_clear_fds(closure);
'''
    s = s[:m.start(1)] + '\n#ifdef __EMSCRIPTEN__' + wasm_body + '#else' + body + '\n#endif' + s[m.end(1):]
    open(p, 'w').write(s)

# -- the server's epoll, which the client library does not use ---------------
p = f'{src}/src/wayland-os.c'
s = open(p).read()
# Its own marker: patch.py has already put a `#ifdef __EMSCRIPTEN__` in this
# file (the transport rename), so "has it been patched" has to ask about the
# epoll guard specifically.
if 'GEX_NO_EPOLL' not in s:
    s = s.replace('#include <sys/epoll.h>',
                  '/* GEX_NO_EPOLL: libwayland-server only, and not built here. */\n'
                  '#ifndef __EMSCRIPTEN__\n#include <sys/epoll.h>\n#endif', 1)
    s = s.replace('int (*wl_epoll_create1)(int flags) = epoll_create1;',
                  '#ifndef __EMSCRIPTEN__\nint (*wl_epoll_create1)(int flags) = epoll_create1;\n#endif', 1)
    m = re.search(r'\nint\nwl_os_epoll_create_cloexec\(void\)\n\{.*?\n\}\n', s, re.S)
    assert m, 'wayland: wl_os_epoll_create_cloexec did not match'
    s = s[:m.start()] + '\n#ifndef __EMSCRIPTEN__' + m.group(0) + '#endif\n' + s[m.end():]
    open(p, 'w').write(s)

p = f'{src}/meson.build'
s = open(p).read()
if 'ffi_dep = host_machine' not in s:
    s = s.replace("\tffi_dep = dependency('libffi')",
                  "\tffi_dep = host_machine.system() == 'emscripten' ? [] : dependency('libffi')", 1)
    s = s.replace("""\tdecls = [
\t\t{ 'header': 'sys/signalfd.h', 'symbol': 'SFD_CLOEXEC' },
\t\t{ 'header': 'sys/timerfd.h', 'symbol': 'TFD_CLOEXEC' },
\t\t{ 'header': 'time.h', 'symbol': 'CLOCK_MONOTONIC' },
\t]""",
"""\tdecls = [
\t\t{ 'header': 'time.h', 'symbol': 'CLOCK_MONOTONIC' },
\t]
\tif host_machine.system() != 'emscripten'
\t\tdecls += [
\t\t\t{ 'header': 'sys/signalfd.h', 'symbol': 'SFD_CLOEXEC' },
\t\t\t{ 'header': 'sys/timerfd.h', 'symbol': 'TFD_CLOEXEC' },
\t\t]
\tendif""", 1)
    assert 'ffi_dep = host_machine' in s, 'wayland: the libffi dependency did not match'
    open(p, 'w').write(s)
print('wayland patched')
PATCH
rm -rf "$OUT/build/wayland"
meson setup "$OUT/build/wayland" "$SRC/wayland" --cross-file "$OUT/emscripten.ini" \
    --prefix="$PREFIX" --buildtype=release -Ddefault_library=static -Dc_thread_count=0 \
    -Dscanner=false -Dtests=false -Ddocumentation=false -Ddtd_validation=false \
    > "$OUT/log-wayland.txt" 2>&1
# The client library alone: `ninja install` would also make the server, which
# wants epoll.
strip_pthread "$OUT/build/wayland"
ninja -C "$OUT/build/wayland" src/libwayland-client.a >> "$OUT/log-wayland.txt" 2>&1
cp "$OUT/build/wayland/src/libwayland-client.a" "$PREFIX/lib/"
cp "$SRC/wayland/src/wayland-client.h" "$SRC/wayland/src/wayland-client-core.h" \
   "$SRC/wayland/src/wayland-util.h" "$OUT/build/wayland/src/wayland-version.h" \
   "$OUT/build/wayland/src/wayland-client-protocol.h" "$PREFIX/include/"
WL_V=$(sed -n 's/.*WAYLAND_VERSION "\([0-9.]*\)".*/\1/p' "$OUT/build/wayland/src/wayland-version.h")
cat > "$PREFIX/lib/pkgconfig/wayland-client.pc" <<EOF
prefix=$PREFIX
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: Wayland Client
Description: Wayland client library, cross-built for wasm32
Version: $WL_V
Cflags: -I\${includedir}
Libs: -L\${libdir} -lwayland-client
EOF

say "a SHA-1 for the libmd path"
mkdir -p "$SRC/sha1"
cat > "$SRC/sha1/sha1.h" <<'EOF'
/* The BSD sha1 interface, which is what xserver's os/xsha1.c expects from
 * libmd or a libc that has it. Emscripten's musl has neither, and the only
 * caller in a wasm Xwayland is a cache it does not use -- but it has to link. */
#ifndef GEX_SHA1_H
#define GEX_SHA1_H
#include <stddef.h>
#include <stdint.h>

typedef struct {
    uint32_t state[5];
    uint64_t count;
    unsigned char buffer[64];
} SHA1_CTX;

void SHA1Init(SHA1_CTX *ctx);
void SHA1Update(SHA1_CTX *ctx, const unsigned char *data, size_t len);
void SHA1Final(unsigned char digest[20], SHA1_CTX *ctx);
#endif
EOF
cat > "$SRC/sha1/sha1.c" <<'EOF'
#include "sha1.h"
#include <string.h>

#define ROL(v, n) (((v) << (n)) | ((v) >> (32 - (n))))

static void block(uint32_t state[5], const unsigned char b[64])
{
    uint32_t w[80], f, k, t, v[5];
    int i;

    for (i = 0; i < 16; i++)
        w[i] = ((uint32_t) b[i * 4] << 24) | ((uint32_t) b[i * 4 + 1] << 16) |
               ((uint32_t) b[i * 4 + 2] << 8) | (uint32_t) b[i * 4 + 3];
    for (; i < 80; i++)
        w[i] = ROL(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);

    memcpy(v, state, sizeof v);
    for (i = 0; i < 80; i++) {
        if (i < 20)      { f = (v[1] & v[2]) | (~v[1] & v[3]); k = 0x5a827999; }
        else if (i < 40) { f = v[1] ^ v[2] ^ v[3];             k = 0x6ed9eba1; }
        else if (i < 60) { f = (v[1] & v[2]) | (v[1] & v[3]) | (v[2] & v[3]); k = 0x8f1bbcdc; }
        else             { f = v[1] ^ v[2] ^ v[3];             k = 0xca62c1d6; }
        t = ROL(v[0], 5) + f + v[4] + k + w[i];
        v[4] = v[3]; v[3] = v[2]; v[2] = ROL(v[1], 30); v[1] = v[0]; v[0] = t;
    }
    for (i = 0; i < 5; i++)
        state[i] += v[i];
}

void SHA1Init(SHA1_CTX *ctx)
{
    ctx->state[0] = 0x67452301; ctx->state[1] = 0xefcdab89;
    ctx->state[2] = 0x98badcfe; ctx->state[3] = 0x10325476;
    ctx->state[4] = 0xc3d2e1f0;
    ctx->count = 0;
}

void SHA1Update(SHA1_CTX *ctx, const unsigned char *data, size_t len)
{
    size_t have = (size_t) ((ctx->count >> 3) & 63), need;

    ctx->count += (uint64_t) len << 3;
    need = 64 - have;
    if (len >= need) {
        if (have) {
            memcpy(ctx->buffer + have, data, need);
            block(ctx->state, ctx->buffer);
            data += need; len -= need; have = 0;
        }
        while (len >= 64) {
            block(ctx->state, data);
            data += 64; len -= 64;
        }
    }
    if (len)
        memcpy(ctx->buffer + have, data, len);
}

void SHA1Final(unsigned char digest[20], SHA1_CTX *ctx)
{
    unsigned char tail[8], one = 0x80, zero = 0;
    size_t i, pad;
    uint64_t bits = ctx->count;

    for (i = 0; i < 8; i++)
        tail[i] = (unsigned char) (bits >> (56 - 8 * i));
    pad = 64 - (size_t) (((ctx->count >> 3) + 8) & 63);
    if (pad == 0) pad = 64;
    SHA1Update(ctx, &one, 1);
    for (i = 1; i < pad; i++)
        SHA1Update(ctx, &zero, 1);
    SHA1Update(ctx, tail, 8);
    for (i = 0; i < 20; i++)
        digest[i] = (unsigned char) (ctx->state[i / 4] >> (24 - 8 * (i % 4)));
}
EOF
(cd "$SRC/sha1" && "$EMCC/emcc" -O2 $PT -c sha1.c -o sha1.o && \
    "$EMCC/emar" rcs "$PREFIX/lib/libmd.a" sha1.o && cp sha1.h "$PREFIX/include/")
# A wrong SHA-1 links as happily as a right one, so check the digest natively.
(cd "$SRC/sha1" && cat > check.c <<'EOF'
#include "sha1.h"
#include <stdio.h>
#include <string.h>
int main(void) {
    SHA1_CTX c; unsigned char d[20]; char hex[41];
    SHA1Init(&c); SHA1Update(&c, (const unsigned char *) "abc", 3); SHA1Final(d, &c);
    for (int i = 0; i < 20; i++) sprintf(hex + i * 2, "%02x", d[i]);
    return strcmp(hex, "a9993e364706816aba3e25717850c26c9cd0d89d") != 0;
}
EOF
cc -O1 -o sha1check check.c sha1.c && ./sha1check) || { echo "the SHA-1 shim is wrong"; exit 1; }

say "xtrans, libXau, libfontenc, libXfont2"
clone libxtrans https://gitlab.freedesktop.org/xorg/lib/libxtrans.git
auto_build libxtrans
# xtrans installs its .c files into the prefix as if they were headers, because
# the X server compiles them in. That is where the X side of the transport is
# redirected, so it has to happen after the install.
python3 "$HERE/patch.py" xtrans "$PREFIX"
clone libXau https://gitlab.freedesktop.org/xorg/lib/libXau.git
auto_build libXau
clone libfontenc https://gitlab.freedesktop.org/xorg/lib/libfontenc.git
auto_build libfontenc
clone libXfont2 https://gitlab.freedesktop.org/xorg/lib/libxfont.git
# No freetype: the server needs core (PCF) fonts to have a default font at all,
# and client-side text is the client's own business (Xft renders in the client
# and arrives as an image).
auto_build libXfont2 --disable-freetype

say "libxkbfile"
clone libxkbfile https://gitlab.freedesktop.org/xorg/lib/libxkbfile.git
python3 - "$SRC/libxkbfile" <<'PATCH'
import sys
p = f'{sys.argv[1]}/src/meson.build'
s = open(p).read()
if 'static_library' not in s:
    s = s.replace("library('xkbfile',", "static_library('xkbfile',", 1)
    # `version` is a shared-library keyword.
    s = s.replace("""        sources: libxkbfile_sources,
        version: '1.0.2'
)""", """        sources: libxkbfile_sources,
)""", 1)
    assert 'static_library' in s, 'libxkbfile: the library target did not match'
    open(p, 'w').write(s)
PATCH
meson_build libxkbfile

say "the keymap, compiled by the host's xkbcomp"
#
# The server compiles a keymap by forking xkbcomp, and there is no fork in a
# tab. So it is compiled here, from the same evdev/pc105/us description
# gex-wayland hands its clients, and embedded in the module; patch.py points
# the server's loader at it. `-nokeymap` then keeps the server from trying to
# use the one the compositor sends over Wayland.
command -v xkbcomp >/dev/null 2>&1 || { echo "need xkbcomp (xorg-xkbcomp)"; exit 1; }
mkdir -p "$OUT/xkb"
cat > "$OUT/xkb/gex.xkb" <<'EOF'
xkb_keymap {
    xkb_keycodes  { include "evdev+aliases(qwerty)" };
    xkb_types     { include "complete"              };
    xkb_compat    { include "complete"              };
    xkb_symbols   { include "pc+us"                 };
    xkb_geometry  { include "pc(pc105)"             };
};
EOF
xkbcomp -xkm "$OUT/xkb/gex.xkb" "$OUT/xkb/gex.xkm" > "$OUT/log-xkb.txt" 2>&1 || {
    echo "xkbcomp failed:"; cat "$OUT/log-xkb.txt"; exit 1; }
printf '  %-28s %9d bytes\n' "gex.xkm" "$(stat -c%s "$OUT/xkb/gex.xkm")"

say "Xwayland $XWAYLAND_TAG"
#
# The link flags are in the cross file rather than a hand-rolled emcc line at
# the end, so meson links the module itself and there is one list of objects,
# not two.
#
#   the memory is SHARED because the page's compositor Worker reads an X
#   window's pixels straight out of it -- that is what makes this build
#   zero-copy -- and shared memory in emscripten comes with -pthread. No
#   thread is ever created (the server's input thread is off): one Worker,
#   blocking in gex_poll, exactly like a guest process parked on the kernel.
#
#   INVOKE_RUN=0 because the Worker sets up the transport before the server
#   runs, and EXIT_RUNTIME=0 because `main` here never returns.
#
#   `-Dc_thread_count=0` is meson's, and it matters: for an emscripten target
#   meson appends -sPTHREAD_POOL_SIZE=<c_thread_count> (4 by default) after our
#   own link flags, so the module preallocates four Workers and blocks on
#   "still waiting on run dependencies: loading-workers" before `main` -- with
#   nothing to load them, since this server never makes a thread.
#
#   built-ins is libXfont2's built-in `fixed` and `cursor`: the server needs a
#   default font to start at all, and shipping font files for that would be
#   silly when the library already carries the two that matter.
if [ "$THREADS" = 0 ]; then
    XWL_LINK="-sJSPI=1 -sALLOW_MEMORY_GROWTH=1"
else
    XWL_LINK="-pthread -sPTHREAD_POOL_SIZE=0 -sALLOW_MEMORY_GROWTH=1"
fi
XWL_LINK="$XWL_LINK -sINITIAL_MEMORY=134217728 -sMAXIMUM_MEMORY=1610612736"
XWL_LINK="$XWL_LINK -sSTACK_SIZE=4194304 -sMODULARIZE=1 -sEXPORT_ES6=1"
XWL_LINK="$XWL_LINK -sENVIRONMENT=web,worker,node -sINVOKE_RUN=0 -sEXIT_RUNTIME=0"
XWL_LINK="$XWL_LINK -sFORCE_FILESYSTEM=1 -sEXPORTED_RUNTIME_METHODS=callMain,FS,ENV,HEAPU8,HEAP32,HEAPU32,wasmMemory,stringToNewUTF8,UTF8ToString,ccall"
XWL_LINK="$XWL_LINK -sEXPORTED_FUNCTIONS=_main,_malloc,_free,_setenv,_getenv"
python3 - "$OUT/emscripten.ini" "$XWL_LINK" <<'PATCH'
import sys
p, extra = sys.argv[1], sys.argv[2]
s = open(p).read()
if '-sMODULARIZE=1' not in s:
    words = ', '.join(f"'{w}'" for w in extra.split())
    s = s.replace("c_link_args = ['-pthread', ", "c_link_args = [" + words + ", ")
    if '-sMODULARIZE=1' not in s:
        s = s.replace("c_link_args = [", "c_link_args = [" + words + ", ", 1)
    assert '-sMODULARIZE=1' in s, 'the cross file link args did not match'
    open(p, 'w').write(s)
PATCH
rm -rf "$OUT/build/xserver"
# The xkb directories are fixed guest paths rather than the prefix's, which
# would put the build host's paths in the module: the server loads only the
# keymap hostx.js writes, and runs no xkbcomp.
# XRes, XINERAMA, DPMS and MIT-SCREEN-SAVER are left at the server's default
# (built): a desktop's clients use them, and some without asking whether the
# server has them, which xcb ends the connection for (Plasma's task manager
# asks XRes for a window's pid).
meson setup "$OUT/build/xserver" "$SRC/xserver" --cross-file "$OUT/emscripten.ini" \
    --prefix="$PREFIX" --buildtype=release -Ddefault_library=static \
    -Dglamor=false -Dxvfb=false -Dglx=false -Dxdmcp=false -Dxdm-auth-1=false \
    -Dsecure-rpc=false -Dipv6=false -Dinput_thread=false -Dsha1=libmd \
    -Ddri3=false -Ddrm=false -Dxwayland_ei=false -Dlisten_tcp=false \
    -Dlisten_local=false -Dxselinux=false -Dsystemd_notify=false -Ddocs=false \
    -Ddevel-docs=false -Dlibdecor=false -Dxv=false -Dmitshm=false \
    -Ddefault_font_path=built-ins -Dc_thread_count=0 \
    -Dxkb_dir=/usr/share/X11/xkb -Dxkb_output_dir=/var/lib/xkb -Dxkb_bin_dir=/usr/bin \
    > "$OUT/log-xserver.txt" 2>&1 || { tail -20 "$OUT/log-xserver.txt"; exit 1; }
strip_pthread "$OUT/build/xserver"
ninja -C "$OUT/build/xserver" >> "$OUT/log-xserver.txt" 2>&1 || {
    grep -E "error:|undefined symbol" "$OUT/log-xserver.txt" | head -20; exit 1; }

# -- the compositor and window manager beside it -----------------------------
#
# The other half of a host-side X server: gex-wayland and the X window manager,
# built as their own module (see hosts/hostwl). Not linked into the server --
# the server's memory is where the pixels are and nothing else needs to be
# there, and Rust's emscripten target would have to be rebuilt with atomics to
# join this link.
say "the host compositor module"
GEX_ROOT="$(cd "$HERE/../.." && pwd)"
# Its panic messages name source files: the tree's and the toolchain's
# paths are remapped, as for the shims.
REMAP="--remap-path-prefix=$GEX_ROOT=/gex --remap-path-prefix=$(rustc --print sysroot)=/rust --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
(cd "$GEX_ROOT" && RUSTFLAGS="$REMAP" cargo build -p gex-hostwl --target wasm32-unknown-unknown --release \
    > "$OUT/log-hostwl.txt" 2>&1) || { tail -20 "$OUT/log-hostwl.txt"; exit 1; }
HOSTWL="$GEX_ROOT/target/wasm32-unknown-unknown/release/gex_hostwl.wasm"

# -- publish -----------------------------------------------------------------
PUB="$GEX_ROOT/page/public/gex/$PUBDIR"
mkdir -p "$PUB"
cp "$OUT/build/xserver/hw/xwayland/Xwayland.js" "$PUB/Xwayland.mjs"
cp "$OUT/build/xserver/hw/xwayland/Xwayland.wasm" "$PUB/"
cp "$HOSTWL" "$PUB/gexhostwl.wasm"
# The keymap is written into the module's filesystem by hostx.js at startup
# rather than embedded at link time: it changes independently of the server, and
# a module that has to be relinked to change a keymap is a worse thing to own.
cp "$OUT/xkb/gex.xkm" "$PUB/"
# The licences of what the server is built from, beside it, for the npm
# package's THIRD-PARTY (page/runtime/build.sh): each source tree's own, and
# the keymap's, compiled from the host's xkeyboard-config.
rm -rf "$PUB/licenses"
mkdir -p "$PUB/licenses"
for n in xserver wayland pixman libxcvt libxtrans libXau libfontenc libXfont2 libxkbfile zlib; do
    f=
    for c in COPYING LICENSE COPYRIGHT; do [ -f "$SRC/$n/$c" ] && { f=$SRC/$n/$c; break; }; done
    [ -n "$f" ] || { echo "no licence file in $SRC/$n" >&2; exit 1; }
    cp "$f" "$PUB/licenses/$n"
done
XKB_COPYRIGHT=${GEX_XKB_COPYRIGHT:-/usr/share/doc/xkb-data/copyright}
[ -f "$XKB_COPYRIGHT" ] || { echo "no $XKB_COPYRIGHT (xkeyboard-config's licence; GEX_XKB_COPYRIGHT names one)" >&2; exit 1; }
cp "$XKB_COPYRIGHT" "$PUB/licenses/xkeyboard-config"

echo
printf '  %-28s %9d bytes\n' "Xwayland.wasm" "$(stat -c%s "$PUB/Xwayland.wasm")"
printf '  %-28s %9d bytes\n' "gexhostwl.wasm" "$(stat -c%s "$PUB/gexhostwl.wasm")"
for a in libpixman-1 libwayland-client libXfont2 libxkbfile libfontenc libXau libz libxcvt; do
    [ -f "$PREFIX/lib/$a.a" ] && printf '  %-28s %9d bytes\n' "$a.a" "$(stat -c%s "$PREFIX/lib/$a.a")"
done
echo
NODE="$(ls -d "$EMSDK/node/"*/bin/node 2>/dev/null | head -1)"
[ -x "$NODE" ] && "$NODE" "$PUB/Xwayland.mjs" -version 2>&1 | head -2 || true
echo
echo "published into $PUB"
echo "built into $OUT"
