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
# It links, the page loads it, and Debian's Xvfb and glxgears both connect to
# it and get a GL context. See the task list for what is left.
#
# Re-running this is cheap. The clone is skipped when the tree is there, and
# meson's build is incremental -- so a change to `virgl/gexvirgl.c`, which is
# the file anyone actually edits, costs a compile and a link rather than the
# ten minutes the first build takes.
set -eu

OUT="${1:-${TMPDIR:-/tmp}/gex-virgl-wasm}"
SRC="$OUT/virglrenderer"
EMSDK="${EMSDK:-$HOME/emsdk}"
DEBUG_FLAGS="${GEX_VIRGL_DEBUG_FLAGS:-}"
# 1.0.0 is the last release before the build grew a hard Vulkan dependency.
VIRGL_TAG="${VIRGL_TAG:-virglrenderer-1.0.0}"

# `pip install --user` puts them here and nothing adds it to a non-login
# shell's PATH, so the script reported meson missing on a machine that had it.
PATH="$HOME/.local/bin:$PATH"
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

# Two changes to the vtest server.
#
# It writes its replies to a file descriptor, and there is no descriptor to
# write to in a tab, so a negative fd means "hand them to gex" and the module
# collects them in a buffer for JavaScript to copy out.
#
# And its callbacks have no GL context: upstream expects virglrenderer to
# bring up its own winsys from EGL, GLX or a DRM node, and this build has
# none of those. gex's three context callbacks are grafted on, so a "GL
# context" becomes a WebGL 2 context on a canvas.
p = f'{src}/vtest/vtest_renderer.c'
s = open(p).read()
if 'gex_virgl_sink' not in s:
    s = s.replace('''static int vtest_block_write(int fd, void *buf, int size)
{''', '''void gex_virgl_sink(const void *data, size_t len);

static int vtest_block_write(int fd, void *buf, int size)
{
   if (fd < 0) {
      gex_virgl_sink(buf, (size_t)size);
      return size;
   }''', 1)
    assert 'gex_virgl_sink' in s, 'vtest_block_write did not match'

if 'gex_create_gl_context' not in s:
    s = s.replace('''static struct virgl_renderer_callbacks renderer_cbs = {
   .version = VIRGL_RENDERER_CALLBACKS_VERSION,
   .write_fence = vtest_write_implicit_fence,
   .get_drm_fd = vtest_get_drm_fd,
   .write_context_fence = vtest_write_context_fence,
};''', '''/* Supplied by gex: the winsys is a WebGL 2 context on a canvas. Note the
 * absence of get_egl_display -- this build has no EGL, so offering one makes
 * virgl_renderer_init refuse it and fail. */
virgl_renderer_gl_context gex_create_gl_context(void *cookie, int scanout,
                                                struct virgl_renderer_gl_ctx_param *param);
void gex_destroy_gl_context(void *cookie, virgl_renderer_gl_context ctx);
int gex_make_current(void *cookie, int scanout, virgl_renderer_gl_context ctx);

static struct virgl_renderer_callbacks renderer_cbs = {
   .version = VIRGL_RENDERER_CALLBACKS_VERSION,
   .write_fence = vtest_write_implicit_fence,
   .get_drm_fd = vtest_get_drm_fd,
   .write_context_fence = vtest_write_context_fence,
   .create_gl_context = gex_create_gl_context,
   .destroy_gl_context = gex_destroy_gl_context,
   .make_current = gex_make_current,
};''', 1)
    assert 'gex_create_gl_context' in s, 'renderer_cbs did not match'
# Both vtest changes land together. Losing this write once cost an afternoon:
# vtest kept its own callbacks, `create_gl_context` stayed NULL, and the
# module died on an indirect call to an empty table slot -- "null function",
# with nothing else printed, because it happened before virglrenderer had
# said anything at all.
open(p, 'w').write(s)

# GL_MAP_UNSYNCHRONIZED_BIT is a promise that nothing else is using the range,
# and emscripten refuses any glMapBufferRange carrying it -- WebGL has no
# unsynchronised mapping to offer. virglrenderer then falls back to a
# glBufferSubData upload, so nothing breaks, but every vertex upload takes the
# slow path and says so. The bit is a hint: dropping it asks for more
# synchronisation, never less.
p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'GEX_NO_UNSYNCHRONIZED' not in s:
    s = s.replace('''      if (!info->synchronized)
         map_flags |= GL_MAP_UNSYNCHRONIZED_BIT;''',
                  '''#ifndef __EMSCRIPTEN__ /* GEX_NO_UNSYNCHRONIZED */
      if (!info->synchronized)
         map_flags |= GL_MAP_UNSYNCHRONIZED_BIT;
#endif''', 1)
    assert 'GEX_NO_UNSYNCHRONIZED' in s, 'the unsynchronized map did not match'
    open(p, 'w').write(s)

# Native GL error queries are cheap enough that virglrenderer validates every
# command after dispatch. In WebGL each getError() crosses into the browser's
# GPU service and synchronizes with it; a race profile showed those checks
# consuming roughly half of the renderer's main-thread samples. Keep the
# checks around setup and transfers, but do not put a synchronous round trip
# after every command in a trusted, locally generated VirGL stream. The
# exported gex_virgl_gl_state() remains an explicit end-to-end error check.
p = f'{src}/src/vrend_decode.c'
s = open(p).read()
if 'GEX_SKIP_PER_COMMAND_GL_ERROR' not in s:
    s = s.replace('''      ret = decode_table[cmd](gdctx->grctx, buf, len);
      if (!vrend_check_no_error(gdctx->grctx) && !ret)
         ret = EINVAL;''',
                  '''      ret = decode_table[cmd](gdctx->grctx, buf, len);
#ifndef __EMSCRIPTEN__ /* GEX_SKIP_PER_COMMAND_GL_ERROR */
      if (!vrend_check_no_error(gdctx->grctx) && !ret)
         ret = EINVAL;
#endif''', 1)
    assert 'GEX_SKIP_PER_COMMAND_GL_ERROR' in s, 'per-command error check did not match'
    open(p, 'w').write(s)

# An index buffer of its own, because WebGL will not let one buffer be both.
#
# WebGL fixes a buffer's kind at its first bind: a buffer bound to
# ELEMENT_ARRAY_BUFFER may never be bound to anything else, and one bound to
# anything else may never be an element buffer. Gallium buffers are typeless,
# and Mesa's upload manager suballocates vertex *and* index data out of one
# resource, so the draw path asked for exactly what WebGL forbids:
#
#   bindBuffer: element array buffers can not be bound to a different target
#   vertexAttribPointer: no ARRAY_BUFFER is bound and offset is non-zero
#   drawElementsInstanced: no buffer is bound to enabled attribute
#
# and the draw produced nothing. glxgears ran at thirty frames a second with
# every frame empty -- `presented=6 lit=0`.
#
# So a resource asked to be an index buffer that was not created as one gets a
# second GL buffer, and the contents are copied into it before the draw.
# COPY_READ and COPY_WRITE are the one pair WebGL lets either kind bind to,
# which is what makes the copy expressible at all.
p = f'{src}/src/vrend_renderer.h'
s = open(p).read()
if 'gex_elem_id' not in s:
    s = s.replace(
        "   GLuint id;\n   GLenum target;\n",
        "   GLuint id;\n   GLenum target;\n\n"
        "   /* A second buffer holding this resource's contents to be drawn as\n"
        "    * indices. WebGL will not let one buffer be both an element array\n"
        "    * and anything else; see web/build-virgl-wasm.sh. */\n"
        "   GLuint gex_elem_id;\n"
        "   GLsizeiptr gex_elem_size;\n", 1)
    assert 'gex_elem_id' in s, 'the resource struct did not match'
    open(p, 'w').write(s)

p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'gex_elem_buffer' not in s:
    s = s.replace(
        "      glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, res->id);",
        "      glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, gex_elem_buffer(res));", 1)
    assert 'gex_elem_buffer(res)' in s, 'the indexed draw bind did not match'
    helper = (
        "/* Copy between two GL buffers of different kinds, through the CPU.\n"
        " *\n"
        " * `glCopyBufferSubData` cannot do it: WebGL refuses a copy whose two\n"
        " * ends are not both element buffers or both not --\n"
        " *\n"
        " *   copyBufferSubData: Cannot copy into an element buffer destination\n"
        " *   from a non-element buffer source\n"
        " *\n"
        " * -- which is the same rule that made the mirror necessary. So the\n"
        " * bytes come back to the host and go out again. */\n"
        "static void gex_mirror_copy(GLuint from, GLuint to, GLenum to_target,\n"
        "                           GLsizeiptr size)\n"
        "{\n"
        "   static void *scratch;\n"
        "   static GLsizeiptr scratch_size;\n"
        "   if (size > scratch_size) {\n"
        "      void *p = realloc(scratch, (size_t)size);\n"
        "      if (!p)\n"
        "         return;\n"
        "      scratch = p;\n"
        "      scratch_size = size;\n"
        "   }\n"
        "   /* `glMapBufferRange` cannot read here either -- emscripten answers\n"
        "    * `glMapBufferRange access does not support MAP_READ`, because WebGL\n"
        "    * has no mapping at all and the emulation only writes. `getBufferSubData`\n"
        "    * is the one way to get bytes out of a WebGL buffer. */\n"
        "   glBindBuffer(GL_COPY_READ_BUFFER, from);\n"
        "   glGetBufferSubData(GL_COPY_READ_BUFFER, 0, size, scratch);\n"
        "   glBindBuffer(GL_COPY_READ_BUFFER, 0);\n"
        "   glBindBuffer(to_target, to);\n"
        "   glBufferSubData(to_target, 0, size, scratch);\n"
        "}\n\n"
        "/* The buffer to bind as this resource's element array.\n"
        " *\n"
        " * WebGL fixes a buffer's kind at its first bind, so a resource created\n"
        " * for anything else needs a second buffer to be indices in, kept up to\n"
        " * date from the first. See web/build-virgl-wasm.sh. */\n"
        "static GLuint gex_elem_buffer(struct vrend_resource *res)\n"
        "{\n"
        "   if (res->target == GL_ELEMENT_ARRAY_BUFFER)\n"
        "      return res->id;\n"
        "   GLsizeiptr size = (GLsizeiptr)res->base.width0;\n"
        "   if (size <= 0)\n"
        "      return res->id;\n"
        "   if (!res->gex_elem_id || res->gex_elem_size != size) {\n"
        "      if (!res->gex_elem_id)\n"
        "         glGenBuffers(1, &res->gex_elem_id);\n"
        "      glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, res->gex_elem_id);\n"
        "      glBufferData(GL_ELEMENT_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_elem_size = size;\n"
        "   }\n"
        "   /* Every draw, rather than when the source changes: the guest writes\n"
        "    * into these constantly and tracking that is a second bug waiting to\n"
        "    * happen. A few kilobytes per draw is not what costs anything. */\n"
        "   gex_mirror_copy(res->id, res->gex_elem_id, GL_ELEMENT_ARRAY_BUFFER, size);\n"
        "   return res->gex_elem_id;\n"
        "}\n\n")
    marker = "static void vrend_draw_bind_vertex_legacy(struct vrend_context *ctx,"
    assert marker in s, 'no place to put the helper'
    s = s.replace(marker, helper + marker, 1)
    open(p, 'w').write(s)

# BGRA is not a thing in WebGL, and the fallback reached for it.
#
# `vrend_transfer_write` uses GL_BGRA whenever the format table has no host
# entry for a resource's format. On GLES there is no BGRA at all -- which is
# why the swizzle-on-upload path immediately below it exists -- so the
# fallback asks for a format the driver has just been told it does not have:
#
#   54x texImage2D: invalid internalformat
#   23x texImage2D: invalid format
p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'GEX_NO_BGRA_FALLBACK' not in s:
    s = s.replace(
        "            glformat = GL_BGRA;\n            gltype = GL_UNSIGNED_BYTE;",
        "            /* GEX_NO_BGRA_FALLBACK: GLES has no BGRA. */\n"
        "            glformat = vrend_state.use_gles ? GL_RGBA : GL_BGRA;\n"
        "            gltype = GL_UNSIGNED_BYTE;", 1)
    assert 'GEX_NO_BGRA_FALLBACK' in s, 'the bgra fallback did not match'
    open(p, 'w').write(s)

# And the same in the other direction.
#
# A resource whose bind flags were exactly VIRGL_BIND_INDEX_BUFFER is created
# as an element buffer, and Mesa then uses the same resource for vertices --
# so the vertex path hits the identical wall from the other side:
#
#   bindBuffer: element array buffers can not be bound to a different target
#   vertexAttribPointer: no ARRAY_BUFFER is bound and offset is non-zero
p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'gex_array_buffer' not in s:
    s = s.replace(
        "      glBindBuffer(GL_ARRAY_BUFFER, res->id);",
        "      glBindBuffer(GL_ARRAY_BUFFER, gex_array_buffer(res));", 1)
    assert 'gex_array_buffer(res)' in s, 'the vertex bind did not match'
    helper = (
        "/* The buffer to bind as this resource's vertex array.\n"
        " *\n"
        " * The mirror of `gex_elem_buffer`, for a resource that was created as\n"
        " * an element buffer and is being used for vertices. */\n"
        "static GLuint gex_array_buffer(struct vrend_resource *res)\n"
        "{\n"
        "   if (res->target != GL_ELEMENT_ARRAY_BUFFER)\n"
        "      return res->id;\n"
        "   GLsizeiptr size = (GLsizeiptr)res->base.width0;\n"
        "   if (size <= 0)\n"
        "      return res->id;\n"
        "   if (!res->gex_array_id || res->gex_array_size != size) {\n"
        "      if (!res->gex_array_id)\n"
        "         glGenBuffers(1, &res->gex_array_id);\n"
        "      glBindBuffer(GL_ARRAY_BUFFER, res->gex_array_id);\n"
        "      glBufferData(GL_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_array_size = size;\n"
        "   }\n"
        "   gex_mirror_copy(res->id, res->gex_array_id, GL_ARRAY_BUFFER, size);\n"
        "   return res->gex_array_id;\n"
        "}\n\n")
    marker = "static GLuint gex_elem_buffer(struct vrend_resource *res)"
    assert marker in s, 'no place to put the vertex helper'
    s = s.replace(marker, helper + marker, 1)
    open(p, 'w').write(s)

p = f'{src}/src/vrend_renderer.h'
s = open(p).read()
if 'gex_array_id' not in s:
    s = s.replace(
        "   GLuint gex_elem_id;\n   GLsizeiptr gex_elem_size;\n",
        "   GLuint gex_elem_id;\n   GLsizeiptr gex_elem_size;\n"
        "   GLuint gex_array_id;\n   GLsizeiptr gex_array_size;\n", 1)
    assert 'gex_array_id' in s, 'the resource struct did not match'
    open(p, 'w').write(s)

# The mirrors are only refilled when the guest has written the buffer.
#
# The copy is a round trip through the host -- out with `getBufferSubData`,
# back with `glBufferSubData` -- and doing it per draw cost about a third of
# the frame rate. Almost none of those copies were needed: a mesh is uploaded
# once and drawn every frame, so the mirror is already right.
#
# `vrend_transfer_write` is the only way a guest changes a buffer's contents,
# so a counter bumped there and remembered by each mirror is exact.
p = f'{src}/src/vrend_renderer.h'
s = open(p).read()
if 'gex_writes' not in s:
    s = s.replace(
        "   GLuint gex_array_id;\n   GLsizeiptr gex_array_size;\n",
        "   GLuint gex_array_id;\n   GLsizeiptr gex_array_size;\n"
        "   /* Bumped by every write to this buffer; each mirror remembers the\n"
        "    * value it was filled at, so an unchanged buffer is not copied. */\n"
        "   uint64_t gex_writes;\n"
        "   uint64_t gex_elem_writes;\n"
        "   uint64_t gex_array_writes;\n", 1)
    assert 'gex_writes' in s, 'the resource struct did not match'
    open(p, 'w').write(s)

p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'gex_writes++' not in s:
    s = s.replace(
        "      glBindBufferARB(res->target, res->id);\n"
        "      data = glMapBufferRange(res->target, info->box->x, info->box->width, map_flags);",
        "      res->gex_writes++;\n"
        "      glBindBufferARB(res->target, res->id);\n"
        "      data = glMapBufferRange(res->target, info->box->x, info->box->width, map_flags);", 1)
    assert 'gex_writes++' in s, 'the buffer write did not match'
    # And the two mirrors consult it.
    s = s.replace(
        "   gex_mirror_copy(res->id, res->gex_elem_id, GL_ELEMENT_ARRAY_BUFFER, size);",
        "   if (res->gex_elem_writes != res->gex_writes + 1) {\n"
        "      gex_mirror_copy(res->id, res->gex_elem_id, GL_ELEMENT_ARRAY_BUFFER, size);\n"
        "      res->gex_elem_writes = res->gex_writes + 1;\n"
        "   }", 1)
    s = s.replace(
        "   gex_mirror_copy(res->id, res->gex_array_id, GL_ARRAY_BUFFER, size);",
        "   if (res->gex_array_writes != res->gex_writes + 1) {\n"
        "      gex_mirror_copy(res->id, res->gex_array_id, GL_ARRAY_BUFFER, size);\n"
        "      res->gex_array_writes = res->gex_writes + 1;\n"
        "   }", 1)
    # A resized mirror is empty whatever the counters say.
    s = s.replace(
        "      glBufferData(GL_ELEMENT_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_elem_size = size;",
        "      glBufferData(GL_ELEMENT_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_elem_size = size;\n"
        "      res->gex_elem_writes = 0;", 1)
    s = s.replace(
        "      glBufferData(GL_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_array_size = size;",
        "      glBufferData(GL_ARRAY_BUFFER, size, NULL, GL_DYNAMIC_DRAW);\n"
        "      res->gex_array_size = size;\n"
        "      res->gex_array_writes = 0;", 1)
    assert 'gex_elem_writes = res->gex_writes + 1' in s, 'the elem mirror did not match'
    assert 'gex_array_writes = res->gex_writes + 1' in s, 'the array mirror did not match'
    open(p, 'w').write(s)

# WebGL 2 is GLES 3.0, but virglrenderer normally maps GLES 3.0 to a desktop
# GLSL feature level of 130.  Guest Mesa consequently exposes OpenGL 2.1 and
# no core profile, even though the renderer already translates Gallium shaders
# to GLSL ES 3.00.  Desktop applications such as SuperTuxKart then fail before
# their first draw while requesting the OpenGL 3.3 core context they require.
# Advertise that translated feature level on this WebGL-only build.  Keep the
# override here rather than upstream: a native GLES host may have a different
# policy, while this renderer is pinned to WebGL 2 above.
p = f'{src}/src/vrend_renderer.c'
s = open(p).read()
if 'GEX_WEBGL_GL33' not in s:
    needle = '''   vrend_fill_caps_glsl_version(gl_ver, gles_ver, caps);
   VREND_DEBUG(dbg_features, NULL, "GLSL support level: %d", caps->v1.glsl_level);'''
    replacement = '''   vrend_fill_caps_glsl_version(gl_ver, gles_ver, caps);
#ifdef __EMSCRIPTEN__ /* GEX_WEBGL_GL33 */
   if (gles_ver >= 30 && caps->v1.glsl_level < 330)
      caps->v1.glsl_level = 330;
#endif
   VREND_DEBUG(dbg_features, NULL, "GLSL support level: %d", caps->v1.glsl_level);'''
    assert needle in s, 'the capset GLSL level did not match'
    s = s.replace(needle, replacement, 1)
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

/* Not GL, and the only non-GL symbol missing: emscripten leaves this out
 * unless the build uses pthreads. Naming a thread is for debuggers. */
int pthread_setname_np(void *thread, const char *name);
int pthread_setname_np(void *thread, const char *name) {
    (void)thread; (void)name; return 0;
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
    src/gallium/libgallium.a src/mesa/libmesa.a vtest/libvtest.a 2>/dev/null ||
ninja -C "$OUT/build" src/libvirglrenderer.a src/libvirgl.a \
    src/gallium/libgallium.a src/mesa/libmesa.a

# -- the module the page loads ----------------------------------------------
#
# FULL_ES3 is what gives the GLES 3 entry points virglrenderer calls -- and
# what makes emscripten route them at a WebGL 2 context rather than emulating
# GLES 2. The desktop-only ones it still cannot supply are in the epoxy shim.
HERE_DIR="$(cd "$(dirname "$0")" && pwd)"
VTEST_SRC="$SRC/vtest/vtest_renderer.c $SRC/vtest/util.c $SRC/vtest/vtest_shm.c"

# The include and define set meson uses for the vtest sources, taken from its
# own compile database rather than guessed. Mesa's utility headers want a
# generated config.h, an endianness it will not infer, and a `compat`
# directory that supplies C11 pieces clang does not have here; getting any of
# them wrong is a `#error`, not a warning.
VTEST_FLAGS="$(cd "$OUT/build" && python3 -c "
import json, shlex
for e in json.load(open('compile_commands.json')):
    if e['file'].endswith('vtest_renderer.c'):
        args = shlex.split(e['command'])
        out = []
        for a in args:
            if a.startswith('-I') and not a[2:].startswith('/'):
                out.append('-I' + '$OUT/build/' + a[2:])
            elif a.startswith(('-I', '-D')):
                out.append(a)
        print(' '.join(out))
        break
")"

# -- the second stub set ----------------------------------------------------
#
# The first set covers what the GLES headers declare and the desktop ones do
# not. This is the mirror: entry points the *desktop* headers declare, so
# virglrenderer compiles, that emscripten's GL library does not implement
# because WebGL 2 has no such call -- glTextureView, glFramebufferTexture1D,
# the multisample-texture family, and so on. virglrenderer reaches them only
# after asking whether the host is desktop GL, and this shim says no.
#
# Discovered by linking rather than listed here: the set follows the version
# of virglrenderer and the version of the SDK, and a list in this file would
# be wrong the first time either moved.
LINK_ARGS="-I$EPOXY/include $VTEST_FLAGS -Wno-pedantic -Wno-c23-extensions \
    -sFULL_ES3=1 -sMAX_WEBGL_VERSION=2 -sMIN_WEBGL_VERSION=2"
OBJS="$HERE_DIR/virgl/gexvirgl.c $VTEST_SRC \
    $OUT/build/src/libvirglrenderer.a $OUT/build/src/libvirgl.a \
    $OUT/build/src/gallium/libgallium.a $OUT/build/src/mesa/libmesa.a \
    $EPOXY/lib/libepoxy.a"

: > "$EPOXY/desktop_stubs.c"
# Twice, because generating a stub can pull a new object out of the archives
# and that object can want something else. Two rounds have always been
# enough; a third costs a link and proves it.
: > "$OUT/link.log"
for _round in 1 2 3; do
    "$EMCC/emcc" -O2 -c "$EPOXY/desktop_stubs.c" -o "$EPOXY/desktop_stubs.o" \
        -I"$EPOXY/include" 2>/dev/null || true
    "$EMCC/emar" rcs "$EPOXY/lib/libepoxy.a" "$EPOXY/epoxy.o" "$EPOXY/stubs.o" \
        ${_round:+$([ -f "$EPOXY/desktop_stubs.o" ] && echo "$EPOXY/desktop_stubs.o")}
    # shellcheck disable=SC2086
    "$EMCC/emcc" -O0 -o "$OUT/probe-link.js" $OBJS $LINK_ARGS \
        -sERROR_ON_UNDEFINED_SYMBOLS=1 -Wl,--error-limit=0 2>>"$OUT/link.log" && break
done
python3 - "$SYSROOT" "$EPOXY" "$OUT/link.log" <<'DESKTOP'
import re, sys
sysroot, out, log = sys.argv[1], sys.argv[2], sys.argv[3]
missing = sorted(set(re.findall(r'undefined symbol: (gl[A-Za-z0-9]+)', open(log).read())))
decls = open(f'{sysroot}/GL/gl.h').read() + open(f'{sysroot}/GL/glext.h').read()
found, unknown = [], []
for name in missing:
    # The word boundary matters: GL/gl.h spells the macro `GLAPIENTRY`, and a
    # bare `APIENTRY` matches its tail, leaving "GL" glued to the return type
    # and every stub declared as `void GL glFoo(...)`.
    m = re.search(r'GLAPI\s+([A-Za-z_][A-Za-z0-9_ *]*?)\s*\b(?:GLAPIENTRY|APIENTRY)\s+'
                  + re.escape(name) + r'\s*\(([^;]*?)\)\s*;', decls)
    (found if m else unknown).append((name, m.group(1).strip(), m.group(2).strip()) if m else name)
body = ['/* Generated by web/build-virgl-wasm.sh. Entry points the desktop GL',
        ' * headers declare that WebGL 2 does not have. Reaching one means',
        ' * virglrenderer did not ask whether the host was desktop GL, so say',
        ' * which call it was rather than rendering nothing. */',
        '#include <stdio.h>', '#include <stdlib.h>',
        '#define GL_GLEXT_PROTOTYPES 1',
        '#include <GL/gl.h>', '#include <GL/glext.h>', '',
        'static void gex_desktop_only(const char *name) {',
        '    fprintf(stderr, "gex-virgl: %s is desktop GL; WebGL 2 has no such call\\n", name);',
        '    abort();', '}', '']
# Some of these are not missing features at all, only missing spellings.
# `glGenBuffersARB` is `glGenBuffers` -- an extension that was promoted into
# core years ago, whose suffixed name virglrenderer still uses and whose
# suffixed symbol emscripten does not export. Aborting on those would stop the
# very first draw of any program, so where the core call exists and takes the
# same arguments, the stub forwards to it instead.
#
# The argument lists must match exactly, compared with parameter names
# stripped. A suffixed call that merely resembles the core one -- and there
# are several -- is not the same call, and forwarding it would be a silent
# wrong answer rather than a loud stop.
# GLES 3.0 and 2.0 only. WebGL 2 *is* GLES 3.0, so a base declared in gl31.h
# or gl32.h is not there to forward to -- glBlendEquationSeparatei is the
# case, and forwarding to it produces a link error rather than a stub that
# says what is missing.
gles = ''
for h in ('GLES3/gl3.h', 'GLES2/gl2.h'):
    try:
        gles += open(f'{sysroot}/{h}').read()
    except OSError:
        pass


# The two families spell the same types differently in places. These are
# typedefs of each other, not distinct types, and treating them as distinct
# rejects real aliases: glDepthRangefOES takes GLclampf where the core
# glDepthRangef takes GLfloat, and they are the same function.
SAME = {'GLclampf': 'GLfloat', 'GLclampd': 'GLdouble', 'GLclampx': 'GLfixed'}


def shape(args):
    """An argument list with the parameter names taken off."""
    out = []
    for a in args.split(','):
        a = a.strip()
        if a in ('', 'void'):
            out.append('void')
            continue
        a = re.sub(r'\b[A-Za-z_][A-Za-z0-9_]*\s*(\[\s*\d*\s*\])?$', '', a).strip()
        a = ' '.join(a.replace('*', ' * ').split())
        out.append(' '.join(SAME.get(w, w) for w in a.split()))
    return out


SUFFIXES = ('ARB', 'EXT', 'OES', 'KHR', 'NV', 'ATI', 'AMD', 'APPLE', 'IMG', 'QCOM')
aliases = {}
for name, ret, args in found:
    base = next((name[: -len(s)] for s in SUFFIXES if name.endswith(s)), None)
    if not base:
        continue
    m = re.search(r'GL_APICALL\s+([^;()]+?)\s*GL_APIENTRY\s+' + re.escape(base)
                  + r'\s*\(([^;()]*)\)\s*;', gles)
    core_ret = ' '.join(SAME.get(w, w) for w in ' '.join(m.group(1).split()).split()) if m else ''
    want_ret = ' '.join(SAME.get(w, w) for w in ret.split())
    if m and shape(m.group(2)) == shape(args) and core_ret == want_ret:
        aliases[name] = base

for name, ret, args in found:
    if name in aliases:
        continue
    b = f'    gex_desktop_only("{name}");\n'
    if ret != 'void':
        b += f'    return ({ret})0;\n'
    body.append(f'{ret} {name}({args}) {{\n{b}}}')

if aliases:
    body.append('\n/* Promoted into core under a different name. Declared here'
                '\n * because the core spelling lives in the GLES headers, which this'
                '\n * file does not include -- including both families is what the'
                '\n * epoxy shim exists to avoid. */')
    for name, base in sorted(aliases.items()):
        ret, args = next((r, a) for n, r, a in found if n == name)
        body.append(f'{ret} {base}({", ".join(shape(args))});')
    body.append('')
    for name, base in sorted(aliases.items()):
        ret, args = next((r, a) for n, r, a in found if n == name)
        # Named parameters, so the call can be forwarded.
        names, decl = [], []
        for i, a in enumerate(shape(args)):
            if a == 'void':
                decl.append('void')
            else:
                names.append(f'a{i}')
                decl.append(f'{a} a{i}')
        call = f'{base}({", ".join(names)});'
        body.append(f'{ret} {name}({", ".join(decl)}) {{\n    '
                    + ('return ' if ret != 'void' else '') + call + '\n}')
open(f'{out}/desktop_stubs.c', 'w').write('\n'.join(body) + '\n')
print(f'desktop stubs: {len(found) - len(aliases)} generated, {len(aliases)} forwarded to core'
      + (f', {len(unknown)} with no prototype: {unknown}' if unknown else ''))
DESKTOP
"$EMCC/emcc" -O2 -c "$EPOXY/desktop_stubs.c" -o "$EPOXY/desktop_stubs.o" -I"$EPOXY/include"
"$EMCC/emar" rcs "$EPOXY/lib/libepoxy.a" "$EPOXY/epoxy.o" "$EPOXY/stubs.o" "$EPOXY/desktop_stubs.o"

# shellcheck disable=SC2086 -- the flag list is intentionally word-split
"$EMCC/emcc" -O2 \
    -o "$OUT/gexvirgl.mjs" \
    "$HERE_DIR/virgl/gexvirgl.c" $VTEST_SRC \
    "$OUT/build/src/libvirglrenderer.a" "$OUT/build/src/libvirgl.a" \
    "$OUT/build/src/gallium/libgallium.a" "$OUT/build/src/mesa/libmesa.a" \
    "$EPOXY/lib/libepoxy.a" \
    $VTEST_FLAGS \
    -Wno-pedantic -Wno-c23-extensions \
    -sFULL_ES3=1 -sMAX_WEBGL_VERSION=2 -sMIN_WEBGL_VERSION=2 \
    -sALLOW_MEMORY_GROWTH=1 -sSTACK_SIZE=1048576 -sEXPORT_ES6=1 -sMODULARIZE=1 \
    -sEXPORTED_RUNTIME_METHODS=ccall,cwrap,HEAPU8,UTF8ToString,GL \
    -sENVIRONMENT=web,worker \
    $DEBUG_FLAGS \
    -sEXPORTED_FUNCTIONS=_gex_virgl_init,_gex_virgl_describe,_gex_virgl_gl_state,_gex_virgl_commands,_gex_virgl_presented,_gex_virgl_feed,_gex_virgl_pump,_gex_virgl_pending,_gex_virgl_replies,_gex_virgl_drain,_gex_virgl_close,_gex_virgl_trace_commands,_gex_virgl_present_texture,_gex_virgl_present_width,_gex_virgl_present_height,_gex_virgl_present_serial,_gex_virgl_present_to_canvas,_malloc,_free

echo
for a in src/libvirglrenderer.a src/libvirgl.a src/gallium/libgallium.a src/mesa/libmesa.a; do
    printf '  %-34s %8d bytes\n' "$a" "$(stat -c%s "$OUT/build/$a")"
done
printf '  %-34s %8d bytes\n' "gexvirgl.wasm" "$(stat -c%s "$OUT/gexvirgl.wasm")"
# And where the page loads it from. Left out before, so a rebuilt renderer sat
# in $OUT while the site kept serving the last one that `build-assets.sh`
# happened to copy -- which looks exactly like a change that did not work.
PUB="$(CDPATH='' cd -- "$(dirname "$0")" && pwd)/public/gex"
if [ -d "$PUB" ]; then
    cp "$OUT/gexvirgl.mjs" "$OUT/gexvirgl.wasm" "$PUB/"
    echo "  published into $PUB"
fi

echo
echo "built into $OUT"
