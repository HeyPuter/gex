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

#include <emscripten/console.h>
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

/* -- the byte streams, one set per client --------------------------------- */

/* Every GL program that opens the renderer's socket is its own vtest client:
 * its own context, its own resource handles, its own stream. The X server is
 * one of them and glxgears is another, and they run at the same time.
 *
 * They used to share one buffer and one context, which worked for exactly as
 * long as there was one of them. The second client's first command is
 * CREATE_RENDERER, the context already existed, so it fell through to the
 * "command this build does not know" arm and the whole stream was refused --
 * after which no reply ever came back, glxgears waited for one forever, and
 * with every process blocked the run went silent rather than failing. Twelve
 * minutes of heartbeat with nothing in it, from one shared variable.
 */
#define GEX_SESSIONS 16

struct gex_session {
    bool live;
    uint8_t *in_buf;
    size_t in_len, in_cap, in_pos;
    uint8_t *out_buf;
    size_t out_len, out_cap;
    struct vtest_context *ctx;
};

static struct gex_session gex_sessions[GEX_SESSIONS];

/* Narrate every command as it starts. */
static bool gex_trace_commands;

EMSCRIPTEN_KEEPALIVE
void gex_virgl_trace_commands(int on) { gex_trace_commands = on != 0; }
/* Which one the renderer is currently running commands for. vtest reads its
 * input and writes its replies through callbacks that take no argument for
 * it, so it has to be here rather than passed. */
static struct gex_session *cur;
static bool short_read;

static struct gex_session *gex_session(int sid) {
    if (sid < 0 || sid >= GEX_SESSIONS) return NULL;
    struct gex_session *s = &gex_sessions[sid];
    if (!s->live) {
        memset(s, 0, sizeof *s);
        s->live = true;
    }
    return s;
}

/* Filled by the patched `vtest_block_write` when the fd is negative. */
void gex_virgl_sink(const void *data, size_t len) {
    if (!cur) return;
    if (cur->out_len + len > cur->out_cap) {
        cur->out_cap = (cur->out_len + len) * 2 + 4096;
        cur->out_buf = realloc(cur->out_buf, cur->out_cap);
    }
    memcpy(cur->out_buf + cur->out_len, data, len);
    cur->out_len += len;
}

static int gex_input_read(struct vtest_input *input, void *buf, int size) {
    (void)input;
    if (!cur || size < 0 || cur->in_pos + (size_t)size > cur->in_len) {
        short_read = true;
        return 0;
    }
    memcpy(buf, cur->in_buf + cur->in_pos, (size_t)size);
    cur->in_pos += (size_t)size;
    return size;
}

static struct vtest_input gex_input = {.read = gex_input_read};

/* -- putting the picture on the canvas ----------------------------------- */

/* vtest has no present. That is not an omission -- it is what vtest is: a
 * test harness whose client reads its pixels back with TRANSFER_GET and
 * compares them. The guest here is an X server, and it does exactly that,
 * copies the result into its own framebuffer, and is perfectly happy for
 * nothing ever to appear on a screen.
 *
 * A demo needs the screen. So every readback of a colour surface is also
 * blitted to the canvas: the resource virglrenderer just rendered into is a
 * GL texture, and `virgl_renderer_resource_get_info` says which one. That
 * makes the canvas a mirror of whatever the guest last looked at, which for
 * glxgears is the frame it is about to hand to X.
 *
 * The blit is upside down on purpose. GL's origin is bottom-left and the
 * canvas's is top-left, so the destination rectangle is inverted rather than
 * the source.
 */
static GLuint gex_blit_fbo;
static int gex_presented;

/* Why a readback did not become a picture. A blank canvas has several
 * causes that look identical from outside -- the guest never read back, it
 * read back a resource that is not a texture, it read back something the
 * size of a cursor -- and each wants a different fix. Counting them apart
 * costs four words and turns a day of guessing into a glance. */
static int gex_present_no_info, gex_present_no_tex, gex_present_small,
    gex_present_incomplete, gex_present_no_handle;
static int gex_submit_present_calls, gex_submit_present_fb0,
    gex_submit_present_small;
/* Presents whose source texture had a pixel in it, and the brightest byte
 * ever seen. `presented` counts blits, which is not the same thing: a
 * renderer that draws nothing still blits, and the canvas stays black while
 * every counter says the frame arrived. This is what tells those apart. */
static int gex_lit;
static int gex_brightest;
static uint32_t gex_present_last_w, gex_present_last_h;

static void gex_present(uint32_t handle) {
    struct virgl_renderer_resource_info info;
    if (virgl_renderer_resource_get_info((int)handle, &info) != 0) {
        gex_present_no_info++;
        return;
    }
    gex_present_last_w = info.width;
    gex_present_last_h = info.height;
    /* Textures only, and only ones big enough to be a window. The X server
     * also reads back cursors and small pixmaps, and a 32x32 cursor stretched
     * over the canvas is not what anyone means by "the picture". */
    if (!info.tex_id) {
        gex_present_no_tex++;
        return;
    }
    if (info.width < 64 || info.height < 64) {
        gex_present_small++;
        return;
    }

    int w, h;
    emscripten_get_canvas_element_size("#gex-gl", &w, &h);
    /* Xwayland also reads pixmaps and intermediate targets. Only a surface
     * matching the output can be the completed application window. */
    if (info.width != (uint32_t)w || info.height != (uint32_t)h) {
        return;
    }
    if (!gex_blit_fbo) glGenFramebuffers(1, &gex_blit_fbo);

    GLint draw_fbo = 0, read_fbo = 0;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw_fbo);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read_fbo);

    glBindFramebuffer(GL_READ_FRAMEBUFFER, gex_blit_fbo);
    glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, info.tex_id, 0);
    if (glCheckFramebufferStatus(GL_READ_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE) {
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
        glDisable(GL_SCISSOR_TEST);
        /* Probe once to distinguish a lit frame from a black presentation.
         * Even a tiny glReadPixels synchronizes the whole WebGL pipeline; a
         * recurring diagnostic probe showed up as a GPU stall in gameplay. */
        static int seen;
        if (seen == 0) {
        unsigned char px[20 * 20 * 4];
        GLint sx = (GLint)info.width / 2 - 10, sy = (GLint)info.height / 2 - 10;
        if (sx < 0) sx = 0;
        if (sy < 0) sy = 0;
        glReadPixels(sx, sy, 20, 20, GL_RGBA, GL_UNSIGNED_BYTE, px);
        for (size_t i = 0; i < sizeof px; i += 4) {
            int v = px[i] > px[i + 1] ? px[i] : px[i + 1];
            if (px[i + 2] > v) v = px[i + 2];
            if (v > gex_brightest) gex_brightest = v;
        }
        if (gex_brightest > 8) gex_lit++;
        }
        seen++;
        glBlitFramebuffer(0, 0, (GLint)info.width, (GLint)info.height,
                          0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        gex_presented++;
    } else {
        gex_present_incomplete++;
    }
    /* Back exactly as it was. virglrenderer keeps its own idea of the bound
     * framebuffer and does not expect anything else to have moved it. */
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)read_fbo);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, (GLuint)draw_fbo);
}

/* Present whatever the guest just rendered into, with no resource handle to go
 * on. gex_present() needs the handle from a TRANSFER_GET body, but a GLES/EGL
 * client that presents through a Wayland/EGL swap never issues one: it renders
 * with SUBMIT_CMD and hands the buffer to the compositor, expecting the pixels
 * to already be on screen. So after a submit we blit the currently-bound draw
 * framebuffer -- the guest's render target -- straight to the canvas. The
 * source rectangle is the viewport, which the client set to its window size.
 * Upside down for the same reason as gex_present: GL is bottom-left, the canvas
 * top-left. */
static void gex_present_current(void) {
    gex_submit_present_calls++;
    GLint fb = 0;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &fb);
    GLint vp[4] = {0, 0, 0, 0};
    glGetIntegerv(GL_VIEWPORT, vp);
    if (fb == 0) { /* default framebuffer holds nothing of the guest's */
        gex_submit_present_fb0++;
        return;
    }
    if (vp[2] < 64 || vp[3] < 64) { /* a scratch pass, not a window */
        gex_submit_present_small++;
        return;
    }

    int w, h;
    emscripten_get_canvas_element_size("#gex-gl", &w, &h);
    /* virglrenderer created and validated this draw target before submitting
     * it. Rechecking framebuffer completeness here is a synchronous WebGL
     * query in a per-frame hot path and has never rejected a target. */
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)fb);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
    glDisable(GL_SCISSOR_TEST);

    static int seen;
    if (seen == 0) {
        unsigned char px[20 * 20 * 4];
        GLint sx = vp[0] + vp[2] / 2 - 10, sy = vp[1] + vp[3] / 2 - 10;
        if (sx < 0) sx = 0;
        if (sy < 0) sy = 0;
        glReadPixels(sx, sy, 20, 20, GL_RGBA, GL_UNSIGNED_BYTE, px);
        for (size_t i = 0; i < sizeof px; i += 4) {
            int v = px[i] > px[i + 1] ? px[i] : px[i + 1];
            if (px[i + 2] > v) v = px[i + 2];
            if (v > gex_brightest) gex_brightest = v;
        }
        if (gex_brightest > 8) gex_lit++;
    }
    seen++;
    glBlitFramebuffer(vp[0], vp[1], vp[0] + vp[2], vp[1] + vp[3],
                      0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    gex_presented++;
    /* Put the guest's framebuffer back as both bindings; virglrenderer expects
     * to find its own target still current. */
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)fb);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, (GLuint)fb);
}

/* The resource handle is the first word of a TRANSFER_GET body, so it can be
 * read without consuming it and without duplicating the header struct. */
/* The resource handle is the first word of a TRANSFER_GET body.
 *
 * Read *after* the command, from where the body started, rather than peeked
 * before it. Peeking ahead only works when the whole body has already
 * arrived; when it has not, the peek returns zero, vtest goes on to read the
 * body correctly as more bytes turn up, and the frame is executed perfectly
 * and never presented. Ten readbacks out of twelve went that way, and
 * because the skip happened before `gex_present` none of the four drop
 * counters noticed -- a black canvas with nothing to say for itself.
 *
 * By the time the command returns, `in_pos` has moved past the body and the
 * bytes are still there: the buffer is only ever compacted by `feed`, which
 * cannot run in the middle of a command.
 */
static int gex_transfer_common(int r, size_t body) {
    if (r < 0 || short_read || !cur) {
        return r;
    }
    uint32_t handle = 0;
    if (body + sizeof handle <= cur->in_len) {
        memcpy(&handle, cur->in_buf + body, sizeof handle);
    }
    if (handle) {
        gex_present(handle);
    } else {
        gex_present_no_handle++;
    }
    return r;
}

static int gex_transfer_get(uint32_t len) {
    size_t body = cur ? cur->in_pos : 0;
    return gex_transfer_common(vtest_transfer_get(len), body);
}

static int gex_transfer_get2(uint32_t len) {
    size_t body = cur ? cur->in_pos : 0;
    return gex_transfer_common(vtest_transfer_get2(len), body);
}

/* The command table, as `vtest_server.c` has it. Kept here rather than
 * reached into, because the server's copy is `static` and its transport is a
 * socket. `VCMD_CREATE_RENDERER` is absent on purpose: it is what makes the
 * context, and `gex_virgl_pump` handles it before this is consulted. */
static int (*const gex_commands[])(uint32_t) = {
    [0] = NULL,
    [VCMD_GET_CAPS] = vtest_send_caps,
    [VCMD_RESOURCE_CREATE] = vtest_create_resource,
    [VCMD_RESOURCE_UNREF] = vtest_resource_unref,
    [VCMD_TRANSFER_GET] = gex_transfer_get,
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
    [VCMD_TRANSFER_GET2] = gex_transfer_get2,
    [VCMD_TRANSFER_PUT2] = vtest_transfer_put2,
    /* Protocol version 3 -- GET_PARAM through SUBMIT_CMD2 -- is behind
     * VIRGL_RENDERER_UNSTABLE_APIS and is not built. It is not missed: gex
     * negotiates version 0, because the later versions pass shared memory by
     * descriptor and there are no descriptors to pass here. */
};
static bool gex_renderer_up;

/* How many of each command has run, indexed the same as the table above. A
 * guest that draws nothing and a guest that draws and is never read back
 * both leave a black canvas; this says which. */
static uint32_t gex_cmd_count[sizeof gex_commands / sizeof *gex_commands];
static const char *const gex_cmd_names[] = {
    [VCMD_GET_CAPS] = "get_caps",
    [VCMD_RESOURCE_CREATE] = "res_create",
    [VCMD_RESOURCE_UNREF] = "res_unref",
    [VCMD_TRANSFER_GET] = "transfer_get",
    [VCMD_TRANSFER_PUT] = "transfer_put",
    [VCMD_SUBMIT_CMD] = "submit_cmd",
    [VCMD_RESOURCE_BUSY_WAIT] = "busy_wait",
    [VCMD_GET_CAPS2] = "get_caps2",
    [VCMD_PING_PROTOCOL_VERSION] = "ping_version",
    [VCMD_PROTOCOL_VERSION] = "version",
    [VCMD_RESOURCE_CREATE2] = "res_create2",
    [VCMD_TRANSFER_GET2] = "transfer_get2",
    [VCMD_TRANSFER_PUT2] = "transfer_put2",
};

/* -- what JavaScript calls ----------------------------------------------- */

EMSCRIPTEN_KEEPALIVE
/* virglrenderer's default logger writes through stdio, and in this module a
 * FILE's write hook is null: the first error it tried to report (es2gears's
 * shader compile failure) died as "null function" inside fputs, saying
 * nothing. Route its messages to the browser console instead. */
static void gex_virgl_log(enum virgl_log_level_flags level, const char *message, void *user) {
    (void)level;
    (void)user;
    emscripten_console_error(message);
}

int gex_virgl_init(int width, int height) {
    virgl_set_log_callback(gex_virgl_log, NULL, NULL);
    EmscriptenWebGLContextAttributes attr;
    emscripten_webgl_init_context_attributes(&attr);
    attr.majorVersion = 2;
    attr.minorVersion = 0;
    attr.alpha = false;
    attr.depth = true;
    attr.stencil = true;
    attr.antialias = false;
    /* Readback is from the guest resource/FBO before it is blitted here. The
     * browser's default framebuffer is presentation-only and can be discarded
     * after compositing; preserving it forces an unnecessary full-frame copy. */
    attr.preserveDrawingBuffer = false;
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
int gex_virgl_feed(int sid, const uint8_t *data, int len) {
    struct gex_session *s = gex_session(sid);
    if (!s) return -1;
    if (len <= 0) {
        return 0;
    }
    /* Compact first, so a long session does not grow the buffer without
     * bound just because its front has been consumed. */
    if (s->in_pos > 0 && s->in_pos == s->in_len) {
        s->in_pos = s->in_len = 0;
    } else if (s->in_pos > 65536) {
        memmove(s->in_buf, s->in_buf + s->in_pos, s->in_len - s->in_pos);
        s->in_len -= s->in_pos;
        s->in_pos = 0;
    }
    if (s->in_len + (size_t)len > s->in_cap) {
        s->in_cap = (s->in_len + (size_t)len) * 2 + 65536;
        s->in_buf = realloc(s->in_buf, s->in_cap);
    }
    memcpy(s->in_buf + s->in_len, data, (size_t)len);
    s->in_len += (size_t)len;
    return 0;
}

/* Run every complete command the buffer holds. Returns the number run, or -1
 * if the stream is unusable. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_pump(int sid) {
    if (!gex_renderer_up) {
        return -1;
    }
    struct gex_session *s = gex_session(sid);
    if (!s) return -1;
    cur = s;
    /* Whose resources and whose GL state the next command means. vtest keeps
     * this in a global of its own, and the last client to run left it
     * pointing at itself. */
    if (s->ctx) vtest_set_current_context(s->ctx);
    int ran = 0;
    int submitted = 0;
    for (;;) {
        size_t began = s->in_pos;
        uint32_t header[VTEST_HDR_SIZE];
        short_read = false;
        if (gex_input_read(&gex_input, &header, sizeof(header)) <= 0) {
            s->in_pos = began;
            break;
        }

        int ret;
        if (!s->ctx) {
            /* The first command must be the one that makes the context. */
            if (header[1] != VCMD_CREATE_RENDERER) {
                return -1;
            }
            ret = vtest_create_context(&gex_input, -1, header[0], &s->ctx);
            if (ret >= 0) {
                ret = vtest_lazy_init_context(s->ctx);
            }
            if (ret >= 0) {
                vtest_set_current_context(s->ctx);
            }
        } else if (header[1] < sizeof gex_commands / sizeof *gex_commands
                   && gex_commands[header[1]] != NULL) {
            vtest_poll_resource_busy_wait();
            gex_cmd_count[header[1]]++;
            /* Said before the command runs, not after.
             *
             * A command that does not return takes the whole run with it:
             * this is called from JavaScript, so no timer fires, the shim's
             * counters do not advance, and the guest is not executing. The
             * question is which command, and a line printed afterwards is a
             * line that never appears. */
            if (gex_trace_commands) {
                const char *nm = header[1] < sizeof gex_cmd_names / sizeof *gex_cmd_names
                                     ? gex_cmd_names[header[1]] : NULL;
                fprintf(stderr, "[virgl] running %s (%u dwords)\n",
                        nm ? nm : "?", header[0]);
            }
            ret = gex_commands[header[1]](header[0]);
            if (header[1] == VCMD_SUBMIT_CMD) submitted = 1;
        } else {
            /* A command this build does not know. The stream is framed by
             * length, but skipping blind would hand the next header's bytes
             * to a command; refusing is the honest answer. */
            return -1;
        }

        if (short_read) {
            /* Only part of this command has arrived. Put it back and wait;
             * running half of it would desynchronise the stream for good. */
            s->in_pos = began;
            break;
        }
        if (ret < 0) {
            return -1;
        }
        ran++;
    }
    /* Process this context's fences, the way `vtest_server_dispatch_clients`
     * does when the context poll fd signals. A client (es2gears' Mesa, and any
     * GLES/EGL client) submits a batch, then WAITS for the submit's fence to
     * signal before it reads back or presents -- not with BUSY_WAIT but by
     * reading the fence notification this poll writes to its stream. Without it
     * the client blocks reading a reply that never comes and the frame never
     * lands. Submission here is synchronous (see the BUSY_WAIT nop note), so the
     * fence is already retired and this only delivers the notification. */
    if (s->ctx) {
        vtest_poll_context(s->ctx);
    }
    if (submitted) {
        gex_present_current();
    }
    return ran;
}

/* What GL makes of the state the replay left behind.
 *
 * virglrenderer reports what it notices, and a frame that comes back blank
 * with nothing said means it noticed nothing -- so ask the other side. An
 * incomplete framebuffer drops every draw silently, and a sticky error from
 * a call made hundreds of commands ago is the usual reason.
 */
EMSCRIPTEN_KEEPALIVE
const char *gex_virgl_gl_state(void) {
    static char out[256];
    GLint draw_fbo = 0, read_fbo = 0, vp[4] = {0, 0, 0, 0};
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw_fbo);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read_fbo);
    glGetIntegerv(GL_VIEWPORT, vp);
    GLenum status = glCheckFramebufferStatus(GL_DRAW_FRAMEBUFFER);
    /* Drained, because GL keeps only the first of each and one left over
     * from setup would mask everything after it. */
    GLenum err = glGetError(), last = err;
    for (int i = 0; i < 32 && last != GL_NO_ERROR; i++) {
        last = glGetError();
        if (last != GL_NO_ERROR) {
            err = last;
        }
    }
    snprintf(out, sizeof out,
             "draw_fbo=%d read_fbo=%d viewport=%d,%d,%dx%d fbo_status=0x%x gl_error=0x%x "
             "presented=%d lit=%d brightest=%d",
             draw_fbo, read_fbo, vp[0], vp[1], vp[2], vp[3], status, err, gex_presented,
             gex_lit, gex_brightest);
    return out;
}

/* Everything the guest asked for, and everything that stopped a readback
 * from reaching the canvas.
 *
 * The two halves answer different questions and both are needed. A stream
 * with no `transfer_get` means the guest is drawing and never looking, so
 * the picture is somewhere the canvas is not. A stream full of them with
 * `no_tex` climbing means the readbacks are of buffers rather than
 * textures, and the blit needs a different source. `small` climbing alone
 * means the only thing being read back is a cursor.
 */
EMSCRIPTEN_KEEPALIVE
const char *gex_virgl_commands(void) {
    static char out[512];
    size_t at = 0;
    for (size_t i = 0; i < sizeof gex_cmd_count / sizeof *gex_cmd_count; i++) {
        if (!gex_cmd_count[i] || at >= sizeof out - 1) continue;
        const char *name = i < sizeof gex_cmd_names / sizeof *gex_cmd_names
                               ? gex_cmd_names[i] : NULL;
        if (name) {
            at += snprintf(out + at, sizeof out - at, "%s%s=%u", at ? " " : "",
                           name, gex_cmd_count[i]);
        } else {
            at += snprintf(out + at, sizeof out - at, "%s#%zu=%u", at ? " " : "",
                           i, gex_cmd_count[i]);
        }
    }
    snprintf(out + at, sizeof out - at,
             "%sdropped: no_handle=%d no_info=%d no_tex=%d small=%d incomplete=%d "
             "last=%ux%u | submit_present: calls=%d fb0=%d small=%d",
             at ? " | " : "", gex_present_no_handle, gex_present_no_info,
             gex_present_no_tex, gex_present_small, gex_present_incomplete,
             gex_present_last_w, gex_present_last_h, gex_submit_present_calls,
             gex_submit_present_fb0, gex_submit_present_small);
    return out;
}

/* How many reply bytes are waiting, and where they are. JavaScript copies
 * them out of the heap and calls `gex_virgl_drain`. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_pending(int sid) {
    struct gex_session *s = gex_session(sid);
    return s ? (int)s->out_len : 0;
}

EMSCRIPTEN_KEEPALIVE
uint8_t *gex_virgl_replies(int sid) {
    struct gex_session *s = gex_session(sid);
    return s ? s->out_buf : NULL;
}

EMSCRIPTEN_KEEPALIVE
void gex_virgl_drain(int sid) {
    struct gex_session *s = gex_session(sid);
    if (s) s->out_len = 0;
}

/* A client has gone. Its context owns GL objects and its buffers own heap,
 * and a demo that opens a window per program would leak both. */
EMSCRIPTEN_KEEPALIVE
void gex_virgl_close(int sid) {
    struct gex_session *s = gex_session(sid);
    if (!s) return;
    if (s->ctx) {
        vtest_set_current_context(s->ctx);
        vtest_destroy_context(s->ctx);
        if (cur == s) cur = NULL;
    }
    free(s->in_buf);
    free(s->out_buf);
    memset(s, 0, sizeof *s);
}
