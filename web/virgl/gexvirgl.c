/* virglrenderer, in a browser tab, on WebGL 2.
 *
 * The guest runs Debian's own Mesa with its Gallium `virgl` driver. That
 * driver encodes a VirGL command stream and speaks the vtest protocol over a
 * socket, and gex serves the other end. Natively the other end is the host's
 * libvirglrenderer on the host's real OpenGL. Here it is this: the same
 * renderer, compiled to WebAssembly, replaying the same stream onto a WebGL 2
 * context.
 *
 * Nothing about the guest changes. It is not told where its GPU is.
 *
 * WHY THIS CAN WORK AT ALL
 *
 * virglrenderer does not require a desktop-GL host -- it has a GLES backend,
 * which is how it runs on phones -- and WebGL 2 is GLES 3.0. The epoxy shim
 * built alongside this reports exactly that, and virglrenderer takes its GLES
 * paths accordingly. What it will not have is compute, geometry and
 * tessellation shaders, or texture buffers, so the capability set it hands the
 * guest is a reduced one and the guest's Mesa falls back to match.
 *
 * THE TRANSPORT
 *
 * Upstream's vtest server reads from a socket and blocks. There are no
 * blocking sockets here, so input is a growable buffer and a `vtest_input`
 * that reads out of it. A command whose bytes have not all arrived must not
 * be executed half way: the read notices it would run past the end, sets
 * `short_read`, and the loop rewinds to where that command started and waits
 * for the rest. Output goes to a buffer for the same reason -- see the
 * `out_fd < 0` patch the build script applies to `vtest_block_write`.
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <emscripten.h>
#include <emscripten/html5.h>
#include <emscripten/html5_webgl.h>

#include <GLES3/gl3.h>

#include "virglrenderer.h"
#include "vtest.h"
#include "vtest_protocol.h"

/* -- the WebGL 2 context, as a winsys ------------------------------------ */

/* virglrenderer can bring up its own EGL from a DRM node. There is no DRM
 * node in a tab, so gex supplies the winsys through the callbacks structure
 * -- the same arrangement the native build uses, for the same reason. Here a
 * "GL context" is a WebGL 2 context on a canvas. */
static EMSCRIPTEN_WEBGL_CONTEXT_HANDLE gex_ctx;

virgl_renderer_gl_context gex_create_gl_context(
    void *cookie, int scanout, struct virgl_renderer_gl_ctx_param *param) {
    (void)cookie;
    (void)scanout;
    (void)param;
    /* One context, shared. WebGL has no equivalent of a share group that can
     * be created after the fact, so every request is answered with the one
     * context -- which is what virglrenderer's sub-contexts want anyway: they
     * expect to see each other's objects. */
    return (virgl_renderer_gl_context)(intptr_t)gex_ctx;
}

void gex_destroy_gl_context(void *cookie, virgl_renderer_gl_context ctx) {
    (void)cookie;
    (void)ctx;
    /* The one context outlives every sub-context; destroying it here would
     * take the canvas with it. */
}

int gex_make_current(void *cookie, int scanout, virgl_renderer_gl_context ctx) {
    (void)cookie;
    (void)scanout;
    return emscripten_webgl_make_context_current(
               (EMSCRIPTEN_WEBGL_CONTEXT_HANDLE)(intptr_t)ctx) == EMSCRIPTEN_RESULT_SUCCESS
               ? 0
               : -1;
}

/* The callbacks are vtest's, not ours: `vtest_init_renderer` registers its
 * own set, and gex's three context callbacks are grafted onto them by the
 * build script. Calling `virgl_renderer_init` here instead would leave
 * vtest's renderer state unbuilt, and the first CREATE_RENDERER would fail
 * with nothing said. */

/* -- the byte streams ---------------------------------------------------- */

static uint8_t *in_buf;
static size_t in_len, in_cap, in_pos;
static bool short_read;

/* Filled by the patched `vtest_block_write` when the fd is negative. */
uint8_t *gex_out_buf;
size_t gex_out_len, gex_out_cap;

void gex_virgl_sink(const void *data, size_t len) {
    if (gex_out_len + len > gex_out_cap) {
        gex_out_cap = (gex_out_len + len) * 2 + 4096;
        gex_out_buf = realloc(gex_out_buf, gex_out_cap);
    }
    memcpy(gex_out_buf + gex_out_len, data, len);
    gex_out_len += len;
}

static int gex_input_read(struct vtest_input *input, void *buf, int size) {
    (void)input;
    if (size < 0 || in_pos + (size_t)size > in_len) {
        short_read = true;
        return 0;
    }
    memcpy(buf, in_buf + in_pos, (size_t)size);
    in_pos += (size_t)size;
    return size;
}

static struct vtest_input gex_input = {.read = gex_input_read};

/* The command table, as `vtest_server.c` has it. Kept here rather than
 * reached into, because the server's copy is `static` and its transport is a
 * socket. `VCMD_CREATE_RENDERER` is absent on purpose: it is what makes the
 * context, and `gex_virgl_pump` handles it before this is consulted. */
static int (*const gex_commands[])(uint32_t) = {
    [0] = NULL,
    [VCMD_GET_CAPS] = vtest_send_caps,
    [VCMD_RESOURCE_CREATE] = vtest_create_resource,
    [VCMD_RESOURCE_UNREF] = vtest_resource_unref,
    [VCMD_TRANSFER_GET] = vtest_transfer_get,
    [VCMD_TRANSFER_PUT] = vtest_transfer_put,
    [VCMD_SUBMIT_CMD] = vtest_submit_cmd,
    /* The non-blocking variant, and it has to be.
     *
     * `vtest_resource_busy_wait` is a `do { ... } while (true)` around
     * `virgl_renderer_get_poll_fd()`: it waits for a fence by polling a
     * descriptor. There is no descriptor in a tab and nothing else runs while
     * this loop does, so it never ends -- the whole module wedges on the
     * first fence a program waits for, which for glxgears is its first frame.
     *
     * Reporting completion immediately is not a shortcut here. `THREAD_SYNC`
     * needs an eventfd and is stripped for want of one, so submission is
     * synchronous: by the time the guest asks, the commands really have run.
     */
    [VCMD_RESOURCE_BUSY_WAIT] = vtest_resource_busy_wait_nop,
    [VCMD_CREATE_RENDERER] = NULL,
    [VCMD_GET_CAPS2] = vtest_send_caps2,
    [VCMD_PING_PROTOCOL_VERSION] = vtest_ping_protocol_version,
    [VCMD_PROTOCOL_VERSION] = vtest_protocol_version,
    /* since protocol version 2 */
    [VCMD_RESOURCE_CREATE2] = vtest_create_resource2,
    [VCMD_TRANSFER_GET2] = vtest_transfer_get2,
    [VCMD_TRANSFER_PUT2] = vtest_transfer_put2,
    /* Protocol version 3 -- GET_PARAM through SUBMIT_CMD2 -- is behind
     * VIRGL_RENDERER_UNSTABLE_APIS and is not built. It is not missed: gex
     * negotiates version 0, because the later versions pass shared memory by
     * descriptor and there are no descriptors to pass here. */
};
static struct vtest_context *gex_context;
static bool gex_renderer_up;

/* -- what JavaScript calls ----------------------------------------------- */

EMSCRIPTEN_KEEPALIVE
int gex_virgl_init(int width, int height) {
    EmscriptenWebGLContextAttributes attr;
    emscripten_webgl_init_context_attributes(&attr);
    attr.majorVersion = 2;
    attr.minorVersion = 0;
    attr.alpha = false;
    attr.depth = true;
    attr.stencil = true;
    attr.antialias = false;
    /* The guest reads its pixels back through TRANSFER_GET rather than
     * looking at the canvas, so the default framebuffer must survive being
     * read after the frame is submitted. */
    attr.preserveDrawingBuffer = true;
    attr.explicitSwapControl = false;

    emscripten_set_canvas_element_size("#gex-gl", width, height);
    gex_ctx = emscripten_webgl_create_context("#gex-gl", &attr);
    if (gex_ctx <= 0) {
        return -1;
    }
    if (emscripten_webgl_make_context_current(gex_ctx) != EMSCRIPTEN_RESULT_SUCCESS) {
        return -2;
    }
    /* Flags zero: the winsys is ours, so virglrenderer must not go looking
     * for EGL or a DRM node of its own. vtest adds the two it insists on. */
    if (vtest_init_renderer(false, 0, NULL) != 0) {
        return -3;
    }
    gex_renderer_up = true;
    return 0;
}

/* What the renderer says it can do, as a string, so the capability set a
 * guest would see is visible without a guest. */
EMSCRIPTEN_KEEPALIVE
const char *gex_virgl_describe(void) {
    static char out[512];
    uint32_t max_ver = 0, max_size = 0;
    virgl_renderer_get_cap_set(1, &max_ver, &max_size);
    const char *vendor = (const char *)glGetString(GL_VENDOR);
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *version = (const char *)glGetString(GL_VERSION);
    snprintf(out, sizeof out,
             "renderer=%s|vendor=%s|version=%s|capset1_version=%u|capset1_size=%u",
             renderer ? renderer : "?", vendor ? vendor : "?", version ? version : "?",
             max_ver, max_size);
    return out;
}

/* Hand over guest bytes. They are appended; nothing is executed until
 * `gex_virgl_pump`. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_feed(const uint8_t *data, int len) {
    if (len <= 0) {
        return 0;
    }
    /* Compact first, so a long session does not grow the buffer without
     * bound just because its front has been consumed. */
    if (in_pos > 0 && in_pos == in_len) {
        in_pos = in_len = 0;
    } else if (in_pos > 65536) {
        memmove(in_buf, in_buf + in_pos, in_len - in_pos);
        in_len -= in_pos;
        in_pos = 0;
    }
    if (in_len + (size_t)len > in_cap) {
        in_cap = (in_len + (size_t)len) * 2 + 65536;
        in_buf = realloc(in_buf, in_cap);
    }
    memcpy(in_buf + in_len, data, (size_t)len);
    in_len += (size_t)len;
    return 0;
}

/* Run every complete command the buffer holds. Returns the number run, or -1
 * if the stream is unusable. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_pump(void) {
    if (!gex_renderer_up) {
        return -1;
    }
    int ran = 0;
    for (;;) {
        size_t began = in_pos;
        uint32_t header[VTEST_HDR_SIZE];
        short_read = false;
        if (gex_input_read(&gex_input, &header, sizeof(header)) <= 0) {
            in_pos = began;
            break;
        }

        int ret;
        if (!gex_context) {
            /* The first command must be the one that makes the context. */
            if (header[1] != VCMD_CREATE_RENDERER) {
                return -1;
            }
            ret = vtest_create_context(&gex_input, -1, header[0], &gex_context);
            if (ret >= 0) {
                ret = vtest_lazy_init_context(gex_context);
            }
            if (ret >= 0) {
                vtest_set_current_context(gex_context);
            }
        } else if (header[1] < sizeof gex_commands / sizeof *gex_commands
                   && gex_commands[header[1]] != NULL) {
            vtest_poll_resource_busy_wait();
            ret = gex_commands[header[1]](header[0]);
        } else {
            /* A command this build does not know. The stream is framed by
             * length, but skipping blind would hand the next header's bytes
             * to a command; refusing is the honest answer. */
            return -1;
        }

        if (short_read) {
            /* Only part of this command has arrived. Put it back and wait;
             * running half of it would desynchronise the stream for good. */
            in_pos = began;
            break;
        }
        if (ret < 0) {
            return -1;
        }
        ran++;
    }
    return ran;
}

/* How many reply bytes are waiting, and where they are. JavaScript copies
 * them out of the heap and calls `gex_virgl_drain`. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_pending(void) {
    return (int)gex_out_len;
}

EMSCRIPTEN_KEEPALIVE
uint8_t *gex_virgl_replies(void) {
    return gex_out_buf;
}

EMSCRIPTEN_KEEPALIVE
void gex_virgl_drain(void) {
    gex_out_len = 0;
}
