#!/bin/sh
# Build virglrenderer for the browser, against WebGL 2.
#
#   web/build-virgl-wasm.sh [outdir]
#
# WHY
#
# Natively, an unmodified OpenGL program in the guest works like this: the
# guest runs Debian's own Mesa, its Gallium `virgl` driver encodes a VirGL
# command stream, and gex hands that to the host's libvirglrenderer, which
# replays it on the host's real OpenGL. No guest driver, no shim library, no
# interposed loader -- `glxgears` at 43 FPS and `glxinfo` reporting OpenGL 4.3.
#
# The page cannot do that, because the page has no host: the shim is wasm and
# the only GPU it can reach is WebGL. What makes the same trick possible there
# is that virglrenderer does not require a desktop-GL host. It has a GLES
# backend -- that is how it runs on phones -- and WebGL 2 is GLES 3.0. So the
# renderer itself can be the thing that crosses: compile it to wasm, point it
# at a WebGL 2 context, and the guest side is unchanged.
#
# WHAT IS AWKWARD ABOUT IT
#
# virglrenderer builds against libepoxy, whose job is to declare the whole of
# GL and resolve entry points at run time. Under emscripten there is nothing
# to resolve -- GLES 3 is linked in, and each call is a call into WebGL -- but
# the declarations still have to exist, because virglrenderer names desktop-GL
# functions in branches it guards with `epoxy_is_desktop_gl()` and never runs
# on a GLES host. It also names the `*_OES`/`*_KHR`/`*_EXT` spellings, which
# the desktop headers do not have.
#
# Including <GL/glext.h> and <GLES2/gl2ext.h> together does not work: they
# define the same PFN typedefs with different signatures. So this generates a
# supplement carrying across only what the desktop headers are missing, and a
# file of stubs for the entry points WebGL 2 has no answer for. A stub that
# runs is a missing guard, not a missing feature, so it aborts and says which
# call it was rather than returning a plausible zero.
#
# STATUS
#
# The renderer core compiles: `libvirglrenderer.a`, wasm objects. It is not
# yet linked into a module the page loads, and nothing is wired to the guest.
# See the task list.
set -eu

OUT="${1:-${TMPDIR:-/tmp}/gex-virgl-wasm}"
SRC="$OUT/virglrenderer"
EMSDK="${EMSDK:-$HOME/emsdk}"
# 1.0.0 is the last release before the build grew a hard Vulkan dependency.
VIRGL_TAG="${VIRGL_TAG:-virglrenderer-1.0.0}"

command -v meson >/dev/null 2>&1 || { echo "need meson (pip install meson ninja)"; exit 1; }
[ -f "$EMSDK/emsdk_env.sh" ] || { echo "no emscripten at $EMSDK"; exit 1; }
# shellcheck disable=SC1091
. "$EMSDK/emsdk_env.sh" >/dev/null 2>&1
EMCC="$EMSDK/upstream/emscripten"
SYSROOT="$EMCC/cache/sysroot/include"

mkdir -p "$OUT"
if [ ! -d "$SRC" ]; then
    git clone --depth 1 --branch "$VIRGL_TAG" \
        https://gitlab.freedesktop.org/virgl/virglrenderer.git "$SRC"
fi

# -- emscripten is not in Mesa's table of operating systems -----------------
#
# virglrenderer vendors a slice of Mesa's utility layer, which decides what it
# is compiling for from a list of `#if defined(__linux__)` and friends and
# `#error`s on anything else. emscripten is close enough to Linux for what
# these files ask of it -- unistd.h, sysconf, pthreads -- with one exception:
# `kcmp`, a Linux syscall used to tell whether two descriptors share a file
# description, which has no equivalent. The non-Linux branch already answers
# that conservatively.
python3 - "$SRC" <<'PATCH'
import re, sys
src = sys.argv[1]

p = f'{src}/src/mesa/util/detect_os.h'
s = open(p).read()
if '__EMSCRIPTEN__' not in s:
    s = s.replace("#if defined(__linux__)\n#define DETECT_OS_LINUX 1",
                  "#if defined(__linux__) || defined(__EMSCRIPTEN__)\n#define DETECT_OS_LINUX 1", 1)
    open(p, 'w').write(s)

p = f'{src}/src/mesa/util/os_file.c'
s = open(p).read()
if '__EMSCRIPTEN__' not in s:
    s = re.sub(r"#if DETECT_OS_LINUX\n", "#if DETECT_OS_LINUX && !defined(__EMSCRIPTEN__)\n", s, count=1)
    open(p, 'w').write(s)
PATCH

# -- the epoxy stand-in -----------------------------------------------------
EPOXY="$OUT/epoxy"
mkdir -p "$EPOXY/include/epoxy" "$EPOXY/lib/pkgconfig"

cat > "$EPOXY/include/epoxy/gl.h" <<'EOF'
/* A stand-in for libepoxy, for building virglrenderer against WebGL 2. */
#ifndef GEX_EPOXY_GL_H
#define GEX_EPOXY_GL_H
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>
#include <epoxy/gles_supplement.h>
#include <stdbool.h>
int epoxy_gl_version(void);
bool epoxy_is_desktop_gl(void);
bool epoxy_has_gl_extension(const char *ext);
#endif
EOF

cat > "$EPOXY/include/epoxy/egl.h" <<'EOF'
#ifndef GEX_EPOXY_EGL_H
#define GEX_EPOXY_EGL_H
#include <EGL/egl.h>
#include <EGL/eglext.h>
#endif
EOF

cat > "$EPOXY/include/epoxy/glx.h" <<'EOF'
/* No GLX in a browser. Declared so the call sites compile; the GLX platform
 * is switched off, so none of them runs. */
#ifndef GEX_EPOXY_GLX_H
#define GEX_EPOXY_GLX_H
#include <stdbool.h>
bool epoxy_has_glx_extension(void *dpy, int screen, const char *ext);
#endif
EOF

cat > "$EPOXY/epoxy.c" <<'EOF'
/* The four functions virglrenderer actually calls, answered from what a
 * WebGL 2 context is: GLES 3.0, not desktop GL. Saying so is what steers
 * virglrenderer down its GLES paths. */
#include <stdbool.h>
#include <string.h>
#include <GLES3/gl32.h>
int epoxy_gl_version(void) { return 30; }
bool epoxy_is_desktop_gl(void) { return false; }
bool epoxy_has_gl_extension(const char *ext) {
    GLint n = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &n);
    for (GLint i = 0; i < n; i++) {
        const char *s = (const char *)glGetStringi(GL_EXTENSIONS, i);
        if (s && strcmp(s, ext) == 0) return true;
    }
    return false;
}
bool epoxy_has_glx_extension(void *dpy, int screen, const char *ext) {
    (void)dpy; (void)screen; (void)ext; return false;
}
EOF

# The supplement and the stubs are generated from the headers emscripten
# ships, so they follow whatever SDK is installed rather than a list checked
# in here and left to rot.
python3 - "$SYSROOT" "$EPOXY" <<'GENERATE'
import re, sys
sysroot, out = sys.argv[1], sys.argv[2]
desktop = open(f'{sysroot}/GL/gl.h').read() + open(f'{sysroot}/GL/glext.h').read()
gles = open(f'{sysroot}/GLES2/gl2ext.h').read() + open(f'{sysroot}/GLES3/gl32.h').read()

have_def = set(re.findall(r'#define\s+(GL_[A-Za-z0-9_]+)', desktop))
have_fn = set(re.findall(r'\b(gl[A-Z][A-Za-z0-9]*)\s*\(', desktop))
have_ty = set(re.findall(r'typedef\s+.*?\(\s*\*\s*(GL[A-Za-z0-9_]+)\s*\)', desktop))

enums = [(n, v) for n, v in re.findall(
    r'#define\s+(GL_[A-Za-z0-9_]+)\s+(0x[0-9A-Fa-f]+|\d+)\s*$', gles, re.M) if n not in have_def]
typedefs = [t for t in re.findall(
    r'^(typedef\s+[^;]*\(\s*GL_APIENTRY\s*\*\s*GL[A-Za-z0-9_]+\s*\)[^;]*;)', gles, re.M)
    if re.search(r'\*\s*(GL[A-Za-z0-9_]+)\s*\)', t).group(1) not in have_ty]
# Bounded to a single statement: args that could swallow a `;` or a `(` would
# run the match on into the next declaration.
fns = [(' '.join(r.split()), n, ' '.join(a.split())) for r, n, a in re.findall(
    r'GL_APICALL\s+([^;()]+?)\s*GL_APIENTRY\s+(gl[A-Za-z0-9]+)\s*\(([^;()]*)\)\s*;', gles)
    if n not in have_fn]
for seq, k in ((enums, 0), (fns, 1)):
    seen = set()
    seq[:] = [x for x in seq if not (x[k] in seen or seen.add(x[k]))]

h = ['/* Generated by web/build-virgl-wasm.sh -- the GLES half of what libepoxy',
     ' * declares, for the parts the desktop GL headers do not have. */',
     '#ifndef GEX_EPOXY_GLES_SUPPLEMENT_H', '#define GEX_EPOXY_GLES_SUPPLEMENT_H',
     '#ifndef GL_APIENTRY', '#define GL_APIENTRY', '#endif',
     '#ifdef __cplusplus', 'extern "C" {', '#endif', '']
h += typedefs + ['']
for n, v in enums:
    h += [f'#ifndef {n}', f'#define {n} {v}', '#endif']
h += ['', '/* Entry points WebGL 2 has no answer for; see stubs.c. */']
h += [f'{r} {n}({a});' for r, n, a in fns]
h += ['', '#ifdef __cplusplus', '}', '#endif', '#endif']
open(f'{out}/include/epoxy/gles_supplement.h', 'w').write('\n'.join(h) + '\n')

s = ['/* Reaching one of these is a missing guard, not a missing feature:',
     ' * virglrenderer asks `epoxy_has_gl_extension` first, and that answers',
     ' * from what the WebGL context reports. So say which call it was rather',
     ' * than returning a plausible zero and rendering nothing. */',
     '#include <stdio.h>', '#include <stdlib.h>', '#include <epoxy/gl.h>', '',
     'static void gex_unavailable(const char *name) {',
     '    fprintf(stderr, "gex-virgl: %s is not in WebGL 2 and was called anyway\\n", name);',
     '    abort();', '}', '']
for r, n, a in fns:
    body = f'    gex_unavailable("{n}");\n'
    if r.strip() != 'void':
        body += f'    return ({r})0;\n'
    s.append(f'{r} {n}({a}) {{\n{body}}}')
open(f'{out}/stubs.c', 'w').write('\n'.join(s) + '\n')
print(f'epoxy shim: {len(enums)} enums, {len(typedefs)} typedefs, {len(fns)} stubs')
GENERATE

cat > "$EPOXY/lib/pkgconfig/epoxy.pc" <<EOF
prefix=$EPOXY
Name: epoxy
Description: a stand-in for libepoxy, for WebGL 2
Version: 1.5.10
Cflags: -I\${prefix}/include
Libs: -L\${prefix}/lib -lepoxy
EOF

"$EMCC/emcc" -O2 -c "$EPOXY/epoxy.c" -o "$EPOXY/epoxy.o" -I"$EPOXY/include"
"$EMCC/emcc" -O2 -c "$EPOXY/stubs.c" -o "$EPOXY/stubs.o" -I"$EPOXY/include"
"$EMCC/emar" rcs "$EPOXY/lib/libepoxy.a" "$EPOXY/epoxy.o" "$EPOXY/stubs.o"

# -- virglrenderer itself ---------------------------------------------------
#
# No platforms: the winsys is supplied through the callbacks structure, the
# same way the native build does it, so virglrenderer needs neither EGL nor
# GBM nor GLX of its own.
cat > "$OUT/emscripten.ini" <<EOF
[binaries]
c = '$EMCC/emcc'
cpp = '$EMCC/em++'
ar = '$EMCC/emar'
strip = '$EMCC/emstrip'
pkg-config = 'pkg-config'

[built-in options]
c_args = ['-I$EPOXY/include', '-Wno-error', '-Wno-pedantic', '-Wno-c23-extensions']
c_link_args = ['-L$EPOXY/lib']

[properties]
needs_exe_wrapper = true

[host_machine]
system = 'emscripten'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
EOF

rm -rf "$OUT/build"
PKG_CONFIG_LIBDIR="$EPOXY/lib/pkgconfig" meson setup "$OUT/build" "$SRC" \
    --cross-file "$OUT/emscripten.ini" \
    -Dplatforms=[] -Ddrm=disabled -Dvenus=false -Dtests=false \
    -Dcheck-gl-errors=false -Ddefault_library=static >/dev/null

# The vtest server target wants a full link and is not what we are after; the
# library is. Build that alone so a missing link-time symbol in an executable
# nobody uses does not fail the build.
ninja -C "$OUT/build" src/libvirglrenderer.a src/libvirgl.a \
    src/gallium/libgallium.a src/mesa/libmesa.a

echo
for a in src/libvirglrenderer.a src/libvirgl.a src/gallium/libgallium.a src/mesa/libmesa.a; do
    printf '  %-34s %8d bytes\n' "$a" "$(stat -c%s "$OUT/build/$a")"
done
echo
echo "built into $OUT/build"
