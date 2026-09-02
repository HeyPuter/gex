/* virglrenderer in a browser tab, hosted by WebGL 2 or MobileGL/WebGPU.
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
#ifndef GEX_VIRGL_WEBGPU
#include <emscripten/html5_webgl.h>
#include <GLES3/gl3.h>
#else
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
namespace MobileGL { void Initialize(); }
#endif

#ifdef __cplusplus
extern "C" {
#endif

#include <emscripten/console.h>
#include "virglrenderer.h"
#include "vtest.h"
#include "vtest_protocol.h"

/* -- the WebGL 2 context, as a winsys ------------------------------------ */

/* virglrenderer can bring up its own EGL from a DRM node. There is no DRM
 * node in a tab, so gex supplies the winsys through the callbacks structure
 * -- the same arrangement the native build uses, for the same reason. Here a
 * "GL context" is a WebGL 2 context on a canvas. */
#ifndef GEX_VIRGL_WEBGPU
static EMSCRIPTEN_WEBGL_CONTEXT_HANDLE gex_ctx;
#else
static EGLDisplay gex_display = EGL_NO_DISPLAY;
static EGLSurface gex_surface = EGL_NO_SURFACE;
static EGLContext gex_ctx = EGL_NO_CONTEXT;
#endif

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
#ifndef GEX_VIRGL_WEBGPU
    return emscripten_webgl_make_context_current(
               (EMSCRIPTEN_WEBGL_CONTEXT_HANDLE)(intptr_t)ctx) == EMSCRIPTEN_RESULT_SUCCESS
               ? 0
               : -1;
#else
    return eglMakeCurrent(gex_display, gex_surface, gex_surface,
                          (EGLContext)(intptr_t)ctx) == EGL_TRUE ? 0 : -1;
#endif
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
        cur->out_buf = (uint8_t *)realloc(cur->out_buf, cur->out_cap);
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

/* A serial for every image copied to the canvas. JavaScript samples this once
 * per animation frame, which coalesces duplicate submit/readback mirrors but
 * still counts either path when it is the only one carrying a new image. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_presented(void) {
    return gex_presented;
}

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
    /* A GL/EGL guest is presented directly from its bound render target after
     * SUBMIT_CMD. Xwayland may subsequently TRANSFER_GET an unrelated backing
     * pixmap; mirroring that readback would overwrite the real frame with the
     * (often black) software-compositor surface. Keep transfer presentation as
     * the fallback for guests that never exercise the submit-side path. */
    if (gex_submit_present_calls != 0) return;
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
#ifdef GEX_VIRGL_WEBGPU
        eglSwapBuffers(gex_display, gex_surface);
#endif
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
/* The frame a compositor in this same context can draw for itself.
 *
 * `gex_present_current` blits the guest's render target onto the canvas,
 * which is the whole picture when the guest is the only thing on screen. In a
 * session it is not: the frame belongs inside a window, among others, and the
 * compositor draws it. Both live in one worker on one WebGL context, so what
 * it needs is the texture -- no readback, no upload, no second copy of the
 * frame anywhere.
 *
 * The texture is the colour attachment of the framebuffer virglrenderer just
 * rendered into, which is a plain GL query rather than anything of
 * virglrenderer's. */
static GLuint gex_present_tex;
static int gex_present_tex_w, gex_present_tex_h;
/* Where a finished frame is copied to, so a compositor never samples a frame
 * that is still being drawn -- one per surface, because the host draws for
 * more than one window at a time.
 *
 * There used to be a single copy for the whole renderer, which is one frame
 * for however many windows were drawing: two GL clients overwrote each
 * other's picture between page frames, and whichever presented last was the
 * one both windows showed. The decoder next door says which surface it is
 * about to present (`gex_virgl_present_surface`), and its id is a device-wide
 * one so two clients cannot collide on it. */
#define GEX_PRESENT_SLOTS 8
struct gex_present_slot {
    unsigned uid;        /* zero for a free slot */
    GLuint copy;         /* the snapshot a compositor draws */
    int w, h;
    unsigned serial;     /* bumped per frame of *this* surface */
    GLuint live;         /* the guest's own target, when a copy is impossible */
};
static struct gex_present_slot gex_slots[GEX_PRESENT_SLOTS];
/* Which surface the frame being presented belongs to. Zero is the surface a
 * client that never named one still has. */
static unsigned gex_present_uid;
/* Whether frames are taken when *asked for* rather than when noticed.
 *
 * This renderer notices a finished frame by watching the command stream,
 * which is the only thing it can do for a guest that runs its own Mesa. When
 * the *host's* Mesa is driving, the decoder knows exactly when a frame is
 * finished and which window it belongs to -- and watching is then strictly
 * worse. Every rule tried on top of watching was wrong for some client:
 * taking a frame on any submit files half-drawn pictures (and, with two
 * clients, files them under the wrong window), while waiting for the flush
 * that carries a swap loses the frame of a client that draws once and stops
 * -- es2tri, whose window stayed black beside a client that kept drawing.
 *
 * So the decoder asks: see `gex_virgl_snapshot_surface`. */
static int gex_present_direct;
static int gex_live_surfaces(void);
static GLuint gex_present_copy_fbo;

/* The slot for a surface, taking a free one if this is the first frame.
 *
 * When every slot is taken the oldest-serial one is reused: a ninth window
 * drawing GL is not a thing that happens, and losing the least active
 * window's frame is better than losing the new one entirely. */
static struct gex_present_slot *gex_slot_for(unsigned uid) {
    struct gex_present_slot *oldest = &gex_slots[0];
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++) {
        if (gex_slots[i].uid == uid) return &gex_slots[i];
        if (!gex_slots[i].uid) {
            gex_slots[i].uid = uid;
            gex_slots[i].serial = 0;
            return &gex_slots[i];
        }
        if (gex_slots[i].serial < oldest->serial) oldest = &gex_slots[i];
    }
    oldest->uid = uid;
    oldest->serial = 0;
    return oldest;
}
/* What went wrong the last time a frame was copied, if anything. */
static GLenum gex_present_error;
/* What the guest's drawable actually is: a texture, a renderbuffer, or
 * nothing this can hand over. */
static GLint gex_present_attach_kind;
static GLuint gex_present_attach_name;
static unsigned gex_present_serial;
/* Whether the canvas still wants the blit. A compositor turns it off: it is
 * going to draw this frame somewhere else on the same canvas, and a blit
 * would overwrite everything else on it. */
static int gex_present_to_canvas = 1;
/* Whether that blit turns the frame over.
 *
 * Which way up a presented frame is depends on who drew it. A guest's own
 * Mesa hands over a window's front buffer, whose rows run top-down; the
 * host's Mesa renders through this renderer's own GL and its rows run
 * bottom-up, the way GL always does. Flipping the second one puts the gears
 * on their heads, so the page says which it has. */
static int gex_present_flip = 1;

EMSCRIPTEN_KEEPALIVE
unsigned gex_virgl_present_texture(void) { return gex_present_tex; }
EMSCRIPTEN_KEEPALIVE
int gex_virgl_present_width(void) { return gex_present_tex_w; }
EMSCRIPTEN_KEEPALIVE
int gex_virgl_present_height(void) { return gex_present_tex_h; }
/* Bumped once per presented frame, so a compositor can tell a new frame from
 * the same texture presented again. */
EMSCRIPTEN_KEEPALIVE
unsigned gex_virgl_present_serial(void) { return gex_present_serial; }
EMSCRIPTEN_KEEPALIVE
void gex_virgl_present_to_canvas(int on) { gex_present_to_canvas = on != 0; }
EMSCRIPTEN_KEEPALIVE
void gex_virgl_present_flip(int on) { gex_present_flip = on != 0; }

/* Which surface the commands that follow belong to: said by the decoder next
 * door whenever the drawable it renders into changes. */
EMSCRIPTEN_KEEPALIVE
void gex_virgl_present_surface(unsigned uid) { gex_present_uid = uid; }

/* Frames are asked for from now on, not noticed: see `gex_present_direct`. */
EMSCRIPTEN_KEEPALIVE
void gex_virgl_present_direct(int on) { gex_present_direct = on != 0; }

/* Keep this surface's frame, now.
 *
 * Called by the decoder the moment a frame is finished -- after its
 * `glFinish`, so everything the client drew has been through this renderer
 * and its own framebuffer is still the bound one. That makes the copy exactly
 * the finished frame of exactly that window, with nothing to infer.
 *
 * Returns zero when there was nothing to take, which for a client that has
 * not drawn yet is ordinary. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_snapshot_surface(unsigned uid) {
    GLint fb = 0;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &fb);
    GLint vp[4] = {0, 0, 0, 0};
    glGetIntegerv(GL_VIEWPORT, vp);
    if (fb == 0 || vp[2] <= 0 || vp[3] <= 0) return 0;
    struct gex_present_slot *slot = gex_slot_for(uid);
    if (!slot->copy) glGenTextures(1, &slot->copy);
    GLint read_was = 0, tex_was = 0;
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read_was);
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &tex_was);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)fb);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glBindTexture(GL_TEXTURE_2D, slot->copy);
    glDisable(GL_SCISSOR_TEST);
    while (glGetError() != GL_NO_ERROR) { }
    /* An unsized `GL_RGBA` on purpose: the guest's drawable is whatever Mesa
     * chose, and a sized destination makes the driver answer
     * INVALID_OPERATION and leave the copy black. Remade every frame for the
     * same reason -- the format is the driver's to choose and need not be the
     * one it chose last time. */
    glCopyTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, vp[0], vp[1], vp[2], vp[3], 0);
    const GLenum copy_error = glGetError();
    if (copy_error == GL_NO_ERROR) {
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        slot->w = vp[2];
        slot->h = vp[3];
        slot->serial++;
        slot->live = 0;
    } else {
        /* Some drawables cannot be read into a plain RGBA8 texture. The
         * guest's own target is then what a compositor gets: it can tear,
         * and it is the picture. */
        gex_present_error = copy_error;
        GLint attached = 0, kind = 0;
        glGetFramebufferAttachmentParameteriv(GL_DRAW_FRAMEBUFFER,
                                              GL_COLOR_ATTACHMENT0,
                                              GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
                                              &kind);
        if (kind == GL_TEXTURE) {
            glGetFramebufferAttachmentParameteriv(GL_DRAW_FRAMEBUFFER,
                                                  GL_COLOR_ATTACHMENT0,
                                                  GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
                                                  &attached);
            slot->live = (GLuint)attached;
            slot->w = vp[2];
            slot->h = vp[3];
            slot->serial++;
        }
    }
    glBindTexture(GL_TEXTURE_2D, (GLuint)tex_was);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)read_was);
    gex_presented++;
    return copy_error == GL_NO_ERROR || slot->live ? 1 : 0;
}

/* One surface's own latest frame. Zero when that surface has never presented
 * -- a window whose client has not drawn yet, or one that has gone. */
EMSCRIPTEN_KEEPALIVE
unsigned gex_virgl_frame_texture(unsigned uid) {
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++)
        if (gex_slots[i].uid == uid)
            return gex_slots[i].copy ? gex_slots[i].copy : gex_slots[i].live;
    return 0;
}
EMSCRIPTEN_KEEPALIVE
int gex_virgl_frame_width(unsigned uid) {
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++)
        if (gex_slots[i].uid == uid) return gex_slots[i].w;
    return 0;
}
EMSCRIPTEN_KEEPALIVE
int gex_virgl_frame_height(unsigned uid) {
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++)
        if (gex_slots[i].uid == uid) return gex_slots[i].h;
    return 0;
}
EMSCRIPTEN_KEEPALIVE
unsigned gex_virgl_frame_serial(unsigned uid) {
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++)
        if (gex_slots[i].uid == uid) return gex_slots[i].serial;
    return 0;
}

/* That window is gone: its snapshot goes back to the driver.
 *
 * A texture per closed window is a leak the size of the window, and a
 * compositor that kept drawing the last frame of a window that had closed was
 * the other half of the same bug. */
EMSCRIPTEN_KEEPALIVE
void gex_virgl_forget_surface(unsigned uid) {
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++) {
        if (gex_slots[i].uid != uid) continue;
        if (gex_slots[i].copy) glDeleteTextures(1, &gex_slots[i].copy);
        gex_slots[i] = (struct gex_present_slot){0};
    }
}

/* How many surfaces have a frame here. Diagnostic. */
static int gex_live_surfaces(void) {
    int n = 0;
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++) if (gex_slots[i].uid) n++;
    return n;
}

/* Put one surface's frame on the canvas, at the canvas's size.
 *
 * For the compositor that has no scene: it hands each window an ordinary
 * canvas and copies this one into it, so several windows can be drawn from
 * one renderer. Returns zero when that surface has no frame yet. */
EMSCRIPTEN_KEEPALIVE
int gex_virgl_show_surface(unsigned uid) {
    GLuint tex = 0;
    int w = 0, h = 0;
    for (int i = 0; i < GEX_PRESENT_SLOTS; i++) {
        if (gex_slots[i].uid != uid) continue;
        tex = gex_slots[i].copy ? gex_slots[i].copy : gex_slots[i].live;
        w = gex_slots[i].w;
        h = gex_slots[i].h;
    }
    if (!tex || w <= 0 || h <= 0) return 0;
    int cw = 0, ch = 0;
    emscripten_get_canvas_element_size("#gex-gl", &cw, &ch);
    if (cw <= 0 || ch <= 0) return 0;
    GLint draw_was = 0, read_was = 0;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw_was);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read_was);
    if (!gex_present_copy_fbo) glGenFramebuffers(1, &gex_present_copy_fbo);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, gex_present_copy_fbo);
    glFramebufferTexture2D(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, tex, 0);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
    glDisable(GL_SCISSOR_TEST);
    while (glGetError() != GL_NO_ERROR) { }
    /* Into the top-left corner of the canvas, at its own size.
     *
     * The top because that is where a `drawImage` of the first w by h pixels
     * reads from, and this canvas is a staging area a page copies out of --
     * it is as large as the desktop and a window is smaller. Not flipped:
     * this is a copy of a frame the host's Mesa rendered through this
     * renderer's own GL, and it is the right way up already, which is the
     * same reason `gex_present_flip` exists. */
    const int dw = cw < w ? cw : w;
    const int dh = ch < h ? ch : h;
    glBlitFramebuffer(0, 0, dw, dh, 0, ch - dh, dw, ch,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);
    const GLenum blit_error = glGetError();
    glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)read_was);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, (GLuint)draw_was);
    if (blit_error != GL_NO_ERROR) {
        gex_present_error = blit_error;
        return 0;
    }
    return 1;
}

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

    /* What the frame *is*, before deciding where it goes. */
    GLint attached = 0, kind = 0;
    glGetFramebufferAttachmentParameteriv(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                          GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE, &kind);
    gex_present_attach_kind = kind;
    if (kind == GL_TEXTURE) {
        glGetFramebufferAttachmentParameteriv(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                              GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
                                              &attached);
        gex_present_tex = (GLuint)attached;
        gex_present_attach_name = (GLuint)attached;
        gex_present_tex_w = vp[2];
        gex_present_tex_h = vp[3];
        gex_present_serial++;
    }
    /* The decoder says when a frame is finished and whose it is, so watching
     * the stream for one would only guess worse. See `gex_present_direct`. */
    if (gex_present_direct) return;
    if (!gex_present_to_canvas) {
        /* A copy of the finished frame, for a compositor to draw.
         *
         * Not the guest's own target: that is the buffer it keeps drawing
         * into, and a compositor sampling it at its own rhythm sees whatever
         * has been rendered so far -- three red bands of a gear that is half
         * drawn. This is one GPU blit at the moment the frame is finished,
         * and no readback: the pixels never leave the GPU. */
        /* A copy of the finished frame, made with `glCopyTexImage2D`.
         *
         * Not a framebuffer blit and not `CopyTexSubImage`: both want the
         * destination's format to match what is being read, and the guest's
         * drawable is whatever Mesa chose -- with a plain RGBA8 destination
         * the driver answered INVALID_OPERATION and left the copy black. An
         * unsized `GL_RGBA` lets it pick a format it can convert into, which
         * is the whole point of the call.
         *
         * The texture is remade every frame rather than sub-copied for the
         * same reason: the format is the driver's to choose and it may not be
         * the one it chose last time. */
        bool use_copy = false;
        struct gex_present_slot *slot = gex_slot_for(gex_present_uid);
        if (!slot->copy) glGenTextures(1, &slot->copy);
        GLint read_was = 0, tex_was = 0;
        glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read_was);
        glGetIntegerv(GL_TEXTURE_BINDING_2D, &tex_was);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)fb);
        glReadBuffer(GL_COLOR_ATTACHMENT0);
        glBindTexture(GL_TEXTURE_2D, slot->copy);
        glDisable(GL_SCISSOR_TEST);
        while (glGetError() != GL_NO_ERROR) { }
        glCopyTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, vp[0], vp[1], vp[2], vp[3], 0);
        GLenum copy_error = glGetError();
        if (copy_error == GL_NO_ERROR) {
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            slot->w = vp[2];
            slot->h = vp[3];
            slot->serial++;
            if (!gex_present_copy_fbo) glGenFramebuffers(1, &gex_present_copy_fbo);
            GLint draw_was = 0;
            glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw_was);
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, gex_present_copy_fbo);
            glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_TEXTURE_2D, slot->copy, 0);
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, (GLuint)draw_was);
        }
        if (copy_error != GL_NO_ERROR) gex_present_error = copy_error;
        glBindTexture(GL_TEXTURE_2D, (GLuint)tex_was);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)read_was);
        /* Whether the snapshot is what a compositor should draw.
         *
         * When the copy works this is the frame as it was when it was
         * finished, which is what a compositor wants. When it does not --
         * some drawables cannot be read into a plain RGBA8 texture, and the
         * driver says so with INVALID_OPERATION -- handing over the guest's
         * own target is better than handing over a black rectangle: it can
         * tear, and it is the picture. */
        use_copy = copy_error == GL_NO_ERROR;
        slot->live = gex_present_tex;
        if (use_copy) {
            gex_present_tex = slot->copy;
            gex_present_tex_w = slot->w;
            gex_present_tex_h = slot->h;
        } else {
            /* No snapshot for this surface: the guest's own target is what a
             * compositor gets, tearing and all -- it is at least the picture. */
            slot->w = vp[2];
            slot->h = vp[3];
            slot->serial++;
        }

        static int probed;
        if (probed++ % 32 == 0) {
            unsigned char px[20 * 20 * 4];
            GLint sx = vp[0] + vp[2] / 2 - 10, sy = vp[1] + vp[3] / 2 - 10;
            if (sx < 0) sx = 0;
            if (sy < 0) sy = 0;
            /* From the copy, not the source: what the compositor will
             * sample is the only thing worth measuring here. */
            GLint was = 0;
            glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &was);
            glBindFramebuffer(GL_READ_FRAMEBUFFER,
                              use_copy ? gex_present_copy_fbo : (GLuint)fb);
            sx = (use_copy ? 0 : vp[0]) + slot->w / 2 - 10;
            sy = (use_copy ? 0 : vp[1]) + slot->h / 2 - 10;
            if (sx < 0) sx = 0;
            if (sy < 0) sy = 0;
            glReadPixels(sx, sy, 20, 20, GL_RGBA, GL_UNSIGNED_BYTE, px);
            GLenum blit_error = glGetError();
            glBindFramebuffer(GL_READ_FRAMEBUFFER, (GLuint)was);
            /* This frame's brightness, not the brightest ever seen: a running
             * maximum says a frame was bright once, which is exactly the
             * question this is being asked to answer. */
            int now = 0;
            for (size_t i = 0; i < sizeof px; i += 4) {
                int v = px[i] > px[i + 1] ? px[i] : px[i + 1];
                if (px[i + 2] > v) v = px[i + 2];
                if (v > now) now = v;
            }
            gex_brightest = now;
            if (now > 8) gex_lit++;
            if (blit_error != GL_NO_ERROR) gex_present_error = blit_error;
        }
        gex_presented++;
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
    // Every thirty-second frame, not only the first. A compositor drawing
    // this texture and getting black has two possible causes -- the frame is
    // black, or the wrong thing is being sampled -- and only the renderer's
    // own reading of its own framebuffer tells them apart.
    if (seen % 32 == 0) {
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
    /* The frame is wherever the viewport says it is: virglrenderer sizes the
     * drawable at init and a smaller window is drawn into the top of it, so
     * the source rectangle is read rather than assumed. The destination is
     * the canvas as it is now, which is how a window sized after init still
     * gets its frame one pixel to one pixel. */
    if (gex_present_flip) {
        glBlitFramebuffer(vp[0], vp[1], vp[0] + vp[2], vp[1] + vp[3],
                          0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    } else {
        glBlitFramebuffer(vp[0], vp[1], vp[0] + vp[2], vp[1] + vp[3],
                          0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    }
#ifdef GEX_VIRGL_WEBGPU
    eglSwapBuffers(gex_display, gex_surface);
#endif
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

int gex_virgl_init(int width, int height, int preserve_drawing_buffer) {
    virgl_set_log_callback(gex_virgl_log, NULL, NULL);
#ifndef GEX_VIRGL_WEBGPU
    EmscriptenWebGLContextAttributes attr;
    emscripten_webgl_init_context_attributes(&attr);
    attr.majorVersion = 2;
    attr.minorVersion = 0;
    attr.alpha = false;
    attr.depth = true;
    attr.stencil = true;
    attr.antialias = false;
    /* Readback is from the guest resource/FBO before it is blitted here. A
     * main-thread canvas is redrawn in the browser's rendering task and can
     * discard its default framebuffer after compositing. OffscreenCanvas is
     * serviced by a sequence of Worker tasks, however: Chromium may expose
     * the post-composite clear between those tasks as a black frame. Preserve
     * that backend until the next completed guest image replaces it. */
    attr.preserveDrawingBuffer = preserve_drawing_buffer != 0;
    attr.explicitSwapControl = false;

    emscripten_set_canvas_element_size("#gex-gl", width, height);
    gex_ctx = emscripten_webgl_create_context("#gex-gl", &attr);
    if (gex_ctx <= 0) {
        return -1;
    }
    if (emscripten_webgl_make_context_current(gex_ctx) != EMSCRIPTEN_RESULT_SUCCESS) {
        return -2;
    }
#else
    (void)preserve_drawing_buffer;
    emscripten_set_canvas_element_size("#gex-gl", width, height);
    MobileGL::Initialize();
    static const EGLint config_attribs[] = {
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_STENCIL_SIZE, 8, EGL_NONE
    };
    gex_display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (gex_display == EGL_NO_DISPLAY ||
        eglInitialize(gex_display, NULL, NULL) != EGL_TRUE ||
        eglBindAPI(EGL_OPENGL_API) != EGL_TRUE) {
        return -1;
    }
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (eglChooseConfig(gex_display, config_attribs, &config, 1,
                        &config_count) != EGL_TRUE || config_count < 1) {
        return -2;
    }
    static const EGLint context_attribs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE
    };
    gex_ctx = eglCreateContext(gex_display, config, EGL_NO_CONTEXT,
                               context_attribs);
    gex_surface = eglCreateWindowSurface(gex_display, config,
                                         (EGLNativeWindowType)1, NULL);
    if (gex_ctx == EGL_NO_CONTEXT || gex_surface == EGL_NO_SURFACE ||
        eglMakeCurrent(gex_display, gex_surface, gex_surface, gex_ctx) != EGL_TRUE) {
        return -2;
    }
#endif
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
        s->in_buf = (uint8_t *)realloc(s->in_buf, s->in_cap);
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
             "presented=%d lit=%d brightest=%d copy_error=0x%x surfaces=%d "
             "attach=0x%x name=%u tex=%u",
             draw_fbo, read_fbo, vp[0], vp[1], vp[2], vp[3], status, err, gex_presented,
             gex_lit, gex_brightest, gex_present_error, gex_live_surfaces(),
             gex_present_attach_kind, gex_present_attach_name, gex_present_tex);
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

#ifdef __cplusplus
}
#endif
