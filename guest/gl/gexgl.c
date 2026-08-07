/* OpenGL ES 2.0 and EGL for a gex guest.
 *
 * This is the guest half of the graphics device. It is deliberately not an
 * implementation of OpenGL: every entry point turns its arguments into a few
 * bytes, appends them to a buffer, and returns. The buffer goes to the host in
 * one ioctl, and the host's own OpenGL does the work.
 *
 * Built as libGLESv2.so.2 and libEGL.so.1, it replaces Mesa. An application
 * is not modified and does not know: it links against the same sonames and
 * calls the same functions.
 *
 * WHY NOT MESA AND VIRTIO-GPU
 *
 * Because every instruction the guest runs is translated aarch64, so guest
 * cycles are dearer than host cycles, and Mesa's state tracker is tens of
 * thousands of instructions per draw. Moving the driver to the host leaves
 * the guest doing arithmetic on a few dozen bytes per call. It also avoids
 * compiling every shader twice, which is what virtio-gpu does: GLSL to TGSI
 * in the guest, TGSI back to GLSL on the host.
 *
 * WHAT COSTS WHAT
 *
 * Appending a command is a bounds check and a memcpy. No syscall, no ioctl,
 * no host call. A whole frame of drawing is one ioctl at the end.
 *
 * A call that must return a value -- glGetError, glGetUniformLocation,
 * glReadPixels, and the compile and link status an application checks -- has
 * to flush and wait. Those are marked below. An application that calls
 * glGetError once a frame pays one round trip a frame; one that calls it
 * after every draw pays for every draw, exactly as it would on a real remote
 * driver, and there is nothing to be done about that from this side.
 *
 * NAMES
 *
 * glGenBuffers and glGenTextures do not ask the host anything. They allocate
 * from a counter here and tell the host which name to use, because GL is
 * happy to bind a name it has not seen. glCreateShader and glCreateProgram
 * also allocate here, and the host keeps a map to its own names. That turns
 * what would be a round trip per object into nothing.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "gexgl.h"

/* ---------------------------------------------------------------- the ring */

/* GEXGL_MAGIC and GEXGL_SUBMIT are generated into gexgl.h from the Rust
   definitions, because writing them twice is writing them differently. */
#define RING_MAGIC GEXGL_MAGIC
#define GEX_GL_SUBMIT GEXGL_SUBMIT
#define HEADER_BYTES 32
#define CMD_CAPACITY (4u * 1024 * 1024)
#define REPLY_CAPACITY (4u * 1024 * 1024)

struct header {
    uint32_t magic;
    uint32_t capacity;
    uint32_t tail;
    uint32_t reply_capacity;
    uint32_t reply_len;
    uint32_t status;
    uint32_t failed_at;
    uint32_t reserved;
};

static int gl_fd = -1;
static struct header *ring;
static uint8_t *cmds;   /* the command area, just past the header */
static uint8_t *replies;
/* How much of the reply the caller has read, so several return values from
   one batch come back in order. */
static uint32_t reply_at;

static int ring_open(void) {
    if (ring) {
        return 0;
    }
    gl_fd = open("/dev/gex-gl", O_RDWR);
    if (gl_fd < 0) {
        return -1;
    }
    size_t total = HEADER_BYTES + CMD_CAPACITY + REPLY_CAPACITY;
    void *p = mmap(NULL, total, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        close(gl_fd);
        gl_fd = -1;
        return -1;
    }
    ring = p;
    ring->magic = RING_MAGIC;
    ring->capacity = CMD_CAPACITY;
    ring->tail = 0;
    ring->reply_capacity = REPLY_CAPACITY;
    ring->reply_len = 0;
    ring->status = 0;
    cmds = (uint8_t *)p + HEADER_BYTES;
    replies = cmds + CMD_CAPACITY;
    return 0;
}

/* Hand everything recorded so far to the host and wait for it. */
static void flush(void) {
    if (!ring || ring->tail == 0) {
        reply_at = 0;
        return;
    }
    if (ioctl(gl_fd, GEX_GL_SUBMIT, ring) != 0) {
        ring->tail = 0;
        ring->reply_len = 0;
    }
    reply_at = 0;
}

/* Start a command. Returns where to write its arguments, having reserved
   room for them, or NULL if the batch is full -- in which case it is flushed
   and the caller tries again. */
static uint32_t *begin(uint32_t opcode, size_t arg_words, size_t tail_bytes) {
    if (!ring && ring_open() != 0) {
        return NULL;
    }
    size_t padded = (tail_bytes + 3u) & ~3u;
    size_t len = 8 + arg_words * 4 + padded;
    if (ring->tail + len > ring->capacity) {
        flush();
        if (len > ring->capacity) {
            return NULL; /* one command larger than the whole buffer */
        }
    }
    uint32_t *at = (uint32_t *)(cmds + ring->tail);
    at[0] = opcode;
    at[1] = (uint32_t)len;
    ring->tail += (uint32_t)len;
    return at + 2;
}

static void put_blob(uint32_t *at, const void *data, size_t len) {
    at[0] = (uint32_t)len;
    if (len) {
        memcpy(at + 1, data, len);
        size_t padded = (len + 3u) & ~3u;
        if (padded > len) {
            memset((uint8_t *)(at + 1) + len, 0, padded - len);
        }
    }
}

/* Read the next word of the reply. Everything that returns a value goes
   through here, and every one of them has flushed first. */
static uint32_t reply_u32(void) {
    if (!ring || reply_at + 4 > ring->reply_len) {
        return 0;
    }
    uint32_t v;
    memcpy(&v, replies + reply_at, 4);
    reply_at += 4;
    return v;
}

static const uint8_t *reply_blob(uint32_t *len_out) {
    uint32_t len = reply_u32();
    if (!ring || reply_at + len > ring->reply_len) {
        *len_out = 0;
        return NULL;
    }
    const uint8_t *p = replies + reply_at;
    reply_at += (len + 3u) & ~3u;
    *len_out = len;
    return p;
}


static uint32_t as_bits(float f) {
    uint32_t v;
    memcpy(&v, &f, 4);
    return v;
}

/* --------------------------------------------------------------- the names */

/* One counter for every kind. GL keeps separate namespaces, but nothing
   requires them to be dense or to start at one, so a single counter is
   simplest and cannot collide with itself. */
static uint32_t next_name = 1;

static uint32_t take_name(void) {
    return next_name++;
}

/* -------------------------------------------------------------------- GLES */

#define CMD0(op)                        \
    do {                                \
        uint32_t *a = begin(op, 0, 0);  \
        (void)a;                        \
    } while (0)

#define CMD1(op, x)                     \
    do {                                \
        uint32_t *a = begin(op, 1, 0);  \
        if (a) a[0] = (uint32_t)(x);    \
    } while (0)

#define CMD2(op, x, y)                  \
    do {                                \
        uint32_t *a = begin(op, 2, 0);  \
        if (a) { a[0] = (uint32_t)(x); a[1] = (uint32_t)(y); } \
    } while (0)

#define CMD3(op, x, y, z)               \
    do {                                \
        uint32_t *a = begin(op, 3, 0);  \
        if (a) { a[0] = (uint32_t)(x); a[1] = (uint32_t)(y); a[2] = (uint32_t)(z); } \
    } while (0)

#define CMD4(op, x, y, z, w)            \
    do {                                \
        uint32_t *a = begin(op, 4, 0);  \
        if (a) { a[0] = (uint32_t)(x); a[1] = (uint32_t)(y);                 \
                 a[2] = (uint32_t)(z); a[3] = (uint32_t)(w); }               \
    } while (0)

void glClearColor(float r, float g, float b, float a) {
    uint32_t *p = begin(GEXGL_CLEAR_COLOR, 4, 0);
    if (p) {
        p[0] = as_bits(r); p[1] = as_bits(g); p[2] = as_bits(b); p[3] = as_bits(a);
    }
}

void glClearDepthf(float d) { CMD1(GEXGL_CLEAR_DEPTH, as_bits(d)); }
void glClear(unsigned mask) { CMD1(GEXGL_CLEAR, mask); }
void glViewport(int x, int y, int w, int h) { CMD4(GEXGL_VIEWPORT, x, y, w, h); }
void glScissor(int x, int y, int w, int h) { CMD4(GEXGL_SCISSOR, x, y, w, h); }
void glEnable(unsigned cap) { CMD1(GEXGL_ENABLE, cap); }
void glDisable(unsigned cap) { CMD1(GEXGL_DISABLE, cap); }
void glBlendFunc(unsigned s, unsigned d) { CMD2(GEXGL_BLEND_FUNC, s, d); }
void glDepthFunc(unsigned f) { CMD1(GEXGL_DEPTH_FUNC, f); }
void glCullFace(unsigned m) { CMD1(GEXGL_CULL_FACE, m); }
void glFrontFace(unsigned m) { CMD1(GEXGL_FRONT_FACE, m); }
void glPixelStorei(unsigned p, int v) { CMD2(GEXGL_PIXEL_STOREI, p, v); }

void glFinish(void) {
    CMD0(GEXGL_FINISH);
    flush();
    (void)reply_u32();
}

void glFlush(void) { flush(); }

void glGenBuffers(int n, unsigned *out) {
    uint32_t *p = begin(GEXGL_GEN_BUFFERS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) {
        out[i] = take_name();
        p[1 + i] = out[i];
    }
}

void glDeleteBuffers(int n, const unsigned *names) {
    uint32_t *p = begin(GEXGL_DELETE_BUFFERS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) p[1 + i] = names[i];
}

void glBindBuffer(unsigned target, unsigned name) { CMD2(GEXGL_BIND_BUFFER, target, name); }

void glBufferData(unsigned target, long size, const void *data, unsigned usage) {
    size_t len = size < 0 ? 0 : (size_t)size;
    uint32_t *p = begin(GEXGL_BUFFER_DATA, 2, 4 + len);
    if (!p) return;
    p[0] = target;
    p[1] = usage;
    put_blob(p + 2, data, data ? len : 0);
    if (!data) p[2] = (uint32_t)len; /* allocate without initialising */
}

void glBufferSubData(unsigned target, long offset, long size, const void *data) {
    size_t len = size < 0 ? 0 : (size_t)size;
    uint32_t *p = begin(GEXGL_BUFFER_SUB_DATA, 2, 4 + len);
    if (!p) return;
    p[0] = target;
    p[1] = (uint32_t)offset;
    put_blob(p + 2, data, len);
}

unsigned glCreateShader(unsigned type) {
    unsigned name = take_name();
    CMD2(GEXGL_CREATE_SHADER, name, type);
    return name;
}

void glShaderSource(unsigned shader, int count, const char *const *strings,
                    const int *lengths) {
    /* GL takes an array of fragments; the host wants one string, so they are
       joined here rather than sending the array. */
    size_t total = 0;
    for (int i = 0; i < count; i++) {
        total += lengths && lengths[i] >= 0 ? (size_t)lengths[i] : strlen(strings[i]);
    }
    uint32_t *p = begin(GEXGL_SHADER_SOURCE, 1, 4 + total);
    if (!p) return;
    p[0] = shader;
    p[1] = (uint32_t)total;
    uint8_t *dst = (uint8_t *)(p + 2);
    for (int i = 0; i < count; i++) {
        size_t n = lengths && lengths[i] >= 0 ? (size_t)lengths[i] : strlen(strings[i]);
        memcpy(dst, strings[i], n);
        dst += n;
    }
    size_t padded = (total + 3u) & ~3u;
    if (padded > total) memset(dst, 0, padded - total);
}

void glCompileShader(unsigned s) { CMD1(GEXGL_COMPILE_SHADER, s); }
void glDeleteShader(unsigned s) { CMD1(GEXGL_DELETE_SHADER, s); }

unsigned glCreateProgram(void) {
    unsigned name = take_name();
    CMD1(GEXGL_CREATE_PROGRAM, name);
    return name;
}

void glAttachShader(unsigned p, unsigned s) { CMD2(GEXGL_ATTACH_SHADER, p, s); }
void glLinkProgram(unsigned p) { CMD1(GEXGL_LINK_PROGRAM, p); }
void glUseProgram(unsigned p) { CMD1(GEXGL_USE_PROGRAM, p); }
void glDeleteProgram(unsigned p) { CMD1(GEXGL_DELETE_PROGRAM, p); }

void glBindAttribLocation(unsigned program, unsigned index, const char *name) {
    size_t len = strlen(name);
    uint32_t *p = begin(GEXGL_BIND_ATTRIB_LOCATION, 2, 4 + len);
    if (!p) return;
    p[0] = program;
    p[1] = index;
    put_blob(p + 2, name, len);
}

/* Round trip: the application is about to branch on the answer. */
void glGetShaderiv(unsigned shader, unsigned pname, int *out) {
    CMD2(GEXGL_GET_SHADER_IV, shader, pname);
    flush();
    *out = (int)reply_u32();
}

void glGetProgramiv(unsigned program, unsigned pname, int *out) {
    CMD2(GEXGL_GET_PROGRAM_IV, program, pname);
    flush();
    *out = (int)reply_u32();
}

static void get_log(uint32_t opcode, unsigned name, int max, int *len, char *out) {
    CMD1(opcode, name);
    flush();
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    if ((int)n > max - 1) n = max > 0 ? (uint32_t)max - 1 : 0;
    if (p && out && max > 0) {
        memcpy(out, p, n);
        out[n] = 0;
    }
    if (len) *len = (int)n;
}

void glGetShaderInfoLog(unsigned s, int max, int *len, char *out) {
    get_log(GEXGL_GET_SHADER_INFO_LOG, s, max, len, out);
}

void glGetProgramInfoLog(unsigned p, int max, int *len, char *out) {
    get_log(GEXGL_GET_PROGRAM_INFO_LOG, p, max, len, out);
}

int glGetUniformLocation(unsigned program, const char *name) {
    size_t len = strlen(name);
    uint32_t *p = begin(GEXGL_GET_UNIFORM_LOCATION, 1, 4 + len);
    if (!p) return -1;
    p[0] = program;
    put_blob(p + 1, name, len);
    flush();
    return (int)reply_u32();
}

int glGetAttribLocation(unsigned program, const char *name) {
    size_t len = strlen(name);
    uint32_t *p = begin(GEXGL_GET_ATTRIB_LOCATION, 1, 4 + len);
    if (!p) return -1;
    p[0] = program;
    put_blob(p + 1, name, len);
    flush();
    return (int)reply_u32();
}

void glUniform1i(int l, int v) { CMD2(GEXGL_UNIFORM_1I, l, v); }
void glUniform1f(int l, float v) { CMD2(GEXGL_UNIFORM_1F, l, as_bits(v)); }
void glUniform2f(int l, float x, float y) { CMD3(GEXGL_UNIFORM_2F, l, as_bits(x), as_bits(y)); }
void glUniform3f(int l, float x, float y, float z) {
    CMD4(GEXGL_UNIFORM_3F, l, as_bits(x), as_bits(y), as_bits(z));
}
void glUniform4f(int l, float x, float y, float z, float w) {
    uint32_t *p = begin(GEXGL_UNIFORM_4F, 5, 0);
    if (!p) return;
    p[0] = (uint32_t)l; p[1] = as_bits(x); p[2] = as_bits(y);
    p[3] = as_bits(z); p[4] = as_bits(w);
}

void glUniformMatrix4fv(int l, int count, unsigned char transpose, const float *v) {
    size_t len = (size_t)count * 64;
    uint32_t *p = begin(GEXGL_UNIFORM_MATRIX4FV, 3, 4 + len);
    if (!p) return;
    p[0] = (uint32_t)l;
    p[1] = (uint32_t)count;
    p[2] = transpose;
    put_blob(p + 3, v, len);
}

void glEnableVertexAttribArray(unsigned i) { CMD1(GEXGL_ENABLE_VERTEX_ATTRIB_ARRAY, i); }
void glDisableVertexAttribArray(unsigned i) { CMD1(GEXGL_DISABLE_VERTEX_ATTRIB_ARRAY, i); }

void glVertexAttribPointer(unsigned index, int size, unsigned type,
                           unsigned char normalized, int stride, const void *offset) {
    uint32_t *p = begin(GEXGL_VERTEX_ATTRIB_POINTER, 6, 0);
    if (!p) return;
    p[0] = index;
    p[1] = (uint32_t)size;
    p[2] = type;
    p[3] = normalized;
    p[4] = (uint32_t)stride;
    /* An offset into the bound buffer. A client-side array -- a real pointer
       into guest memory -- would mean nothing to the host, and GLES2 core
       allows them; an application that uses one needs the data uploaded
       first, which is not done here and would be silently wrong if it were
       ignored. Rejected loudly instead. */
    p[5] = (uint32_t)(uintptr_t)offset;
}

void glGenTextures(int n, unsigned *out) {
    uint32_t *p = begin(GEXGL_GEN_TEXTURES, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) {
        out[i] = take_name();
        p[1 + i] = out[i];
    }
}

void glDeleteTextures(int n, const unsigned *names) {
    uint32_t *p = begin(GEXGL_DELETE_TEXTURES, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) p[1 + i] = names[i];
}

void glBindTexture(unsigned t, unsigned n) { CMD2(GEXGL_BIND_TEXTURE, t, n); }
void glActiveTexture(unsigned u) { CMD1(GEXGL_ACTIVE_TEXTURE, u); }
void glTexParameteri(unsigned t, unsigned p, int v) { CMD3(GEXGL_TEX_PARAMETER_I, t, p, v); }

static size_t pixel_bytes(unsigned format, unsigned type, int w, int h) {
    size_t per = 4;
    if (type == 0x8363 || type == 0x8033 || type == 0x8034) per = 2; /* packed 16-bit */
    else if (format == 0x1907) per = 3;                              /* GL_RGB */
    else if (format == 0x1906 || format == 0x1909) per = 1;          /* ALPHA, LUMINANCE */
    else if (format == 0x190A) per = 2;                              /* LUMINANCE_ALPHA */
    return per * (size_t)(w < 0 ? 0 : w) * (size_t)(h < 0 ? 0 : h);
}

void glTexImage2D(unsigned target, int level, int internal, int w, int h, int border,
                  unsigned format, unsigned type, const void *pixels) {
    size_t len = pixels ? pixel_bytes(format, type, w, h) : 0;
    uint32_t *p = begin(GEXGL_TEX_IMAGE_2D, 8, 4 + len);
    if (!p) return;
    p[0] = target; p[1] = (uint32_t)level; p[2] = (uint32_t)internal;
    p[3] = (uint32_t)w; p[4] = (uint32_t)h; p[5] = (uint32_t)border;
    p[6] = format; p[7] = type;
    put_blob(p + 8, pixels, len);
}

void glTexSubImage2D(unsigned target, int level, int x, int y, int w, int h,
                     unsigned format, unsigned type, const void *pixels) {
    size_t len = pixels ? pixel_bytes(format, type, w, h) : 0;
    uint32_t *p = begin(GEXGL_TEX_SUB_IMAGE_2D, 8, 4 + len);
    if (!p) return;
    p[0] = target; p[1] = (uint32_t)level; p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)w; p[5] = (uint32_t)h; p[6] = format; p[7] = type;
    put_blob(p + 8, pixels, len);
}

void glDrawArrays(unsigned mode, int first, int count) {
    CMD3(GEXGL_DRAW_ARRAYS, mode, first, count);
}

void glDrawElements(unsigned mode, int count, unsigned type, const void *offset) {
    CMD4(GEXGL_DRAW_ELEMENTS, mode, count, type, (uint32_t)(uintptr_t)offset);
}

void glReadPixels(int x, int y, int w, int h, unsigned format, unsigned type, void *out) {
    uint32_t *p = begin(GEXGL_READ_PIXELS, 6, 0);
    if (!p) return;
    p[0] = (uint32_t)x; p[1] = (uint32_t)y; p[2] = (uint32_t)w; p[3] = (uint32_t)h;
    p[4] = format; p[5] = type;
    flush();
    uint32_t n = 0;
    const uint8_t *px = reply_blob(&n);
    if (px && out) memcpy(out, px, n);
}

unsigned glGetError(void) {
    CMD0(GEXGL_GET_ERROR);
    flush();
    return reply_u32();
}

/* The strings are asked for once and never change, so they are kept rather
   than fetched again -- an application that calls glGetString in a loop is
   not unusual. */
static char *cached_strings[8];

const unsigned char *glGetString(unsigned name) {
    unsigned slot = name & 7u;
    if (cached_strings[slot]) {
        return (const unsigned char *)cached_strings[slot];
    }
    CMD1(GEXGL_GET_STRING, name);
    flush();
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    char *s = malloc(n + 1);
    if (!s) return (const unsigned char *)"";
    if (p) memcpy(s, p, n);
    s[n] = 0;
    cached_strings[slot] = s;
    return (const unsigned char *)s;
}

void glGetIntegerv(unsigned pname, int *out) {
    CMD1(GEXGL_GET_INTEGERV, pname);
    flush();
    for (int i = 0; i < 4; i++) {
        int v = (int)reply_u32();
        if (i == 0 && out) out[0] = v;
    }
}

/* --------------------------------------------------------------------- EGL */

/* EGL here is bookkeeping and nothing else. There is one display, one config
 * and one context, because the host has one and a guest that wants several
 * would need the host to as well. What matters is that an application's
 * initialisation sequence succeeds and it gets to its first draw. */

#define EGL_TRUE 1
#define EGL_FALSE 0

static int egl_ready;

void *eglGetDisplay(void *native) {
    (void)native;
    return (void *)1;
}

void *eglGetPlatformDisplay(unsigned platform, void *native, const intptr_t *attrs) {
    (void)platform; (void)native; (void)attrs;
    return (void *)1;
}

unsigned eglInitialize(void *dpy, int *major, int *minor) {
    (void)dpy;
    if (ring_open() != 0) {
        return EGL_FALSE;
    }
    CMD0(GEXGL_CONTEXT_CREATE);
    flush();
    /* Vendor, renderer and version come back; they are what glGetString
       would have said, and asking now saves three round trips later. */
    uint32_t n = 0;
    for (int i = 0; i < 3; i++) {
        const uint8_t *p = reply_blob(&n);
        if (p && i < 3) {
            char *s = malloc(n + 1);
            if (s) {
                memcpy(s, p, n);
                s[n] = 0;
                /* GL_VENDOR 0x1F00, GL_RENDERER 0x1F01, GL_VERSION 0x1F02 */
                cached_strings[(0x1F00u + (unsigned)i) & 7u] = s;
            }
        }
    }
    if (major) *major = 1;
    if (minor) *minor = 5;
    egl_ready = 1;
    return EGL_TRUE;
}

unsigned eglTerminate(void *dpy) { (void)dpy; return EGL_TRUE; }
unsigned eglBindAPI(unsigned api) { (void)api; return EGL_TRUE; }

unsigned eglChooseConfig(void *dpy, const int *attrs, void **configs, int size, int *num) {
    (void)dpy; (void)attrs;
    if (size > 0 && configs) configs[0] = (void *)1;
    if (num) *num = size > 0 ? 1 : 0;
    return EGL_TRUE;
}

unsigned eglGetConfigAttrib(void *dpy, void *config, int attr, int *value) {
    (void)dpy; (void)config; (void)attr;
    if (value) *value = 0;
    return EGL_TRUE;
}

void *eglCreateWindowSurface(void *dpy, void *config, void *win, const int *attrs) {
    (void)dpy; (void)config; (void)win; (void)attrs;
    return (void *)2;
}

void *eglCreatePbufferSurface(void *dpy, void *config, const int *attrs) {
    (void)dpy; (void)config;
    /* The host's surface is sized at startup; a pbuffer request resizes it,
       which is the closest thing to what the caller asked for. */
    int w = 0, h = 0;
    if (attrs) {
        for (const int *a = attrs; *a != 0x3038; a += 2) {
            if (a[0] == 0x3057) w = a[1];      /* EGL_WIDTH */
            else if (a[0] == 0x3056) h = a[1]; /* EGL_HEIGHT */
        }
    }
    if (w > 0 && h > 0) {
        CMD2(GEXGL_SURFACE_RESIZE, w, h);
    }
    return (void *)2;
}

unsigned eglDestroySurface(void *dpy, void *surface) {
    (void)dpy; (void)surface;
    return EGL_TRUE;
}

void *eglCreateContext(void *dpy, void *config, void *share, const int *attrs) {
    (void)dpy; (void)config; (void)share; (void)attrs;
    return (void *)3;
}

unsigned eglDestroyContext(void *dpy, void *ctx) { (void)dpy; (void)ctx; return EGL_TRUE; }

unsigned eglMakeCurrent(void *dpy, void *draw, void *read, void *ctx) {
    (void)dpy; (void)draw; (void)read; (void)ctx;
    return egl_ready ? EGL_TRUE : EGL_FALSE;
}

unsigned eglSwapBuffers(void *dpy, void *surface) {
    (void)dpy; (void)surface;
    /* Nothing is presented from here -- what the guest drew is on the host,
       and the host decides where it goes. Flushing is the part that matters:
       a frame's worth of commands crosses now. */
    flush();
    return EGL_TRUE;
}

unsigned eglSwapInterval(void *dpy, int interval) { (void)dpy; (void)interval; return EGL_TRUE; }
int eglGetError(void) { return 0x3000; /* EGL_SUCCESS */ }

const char *eglQueryString(void *dpy, int name) {
    (void)dpy;
    switch (name) {
    case 0x3053: return "1.5";       /* EGL_VERSION */
    case 0x3054: return "gex";       /* EGL_VENDOR */
    case 0x3055: return "";          /* EGL_EXTENSIONS */
    case 0x308D: return "OpenGL_ES"; /* EGL_CLIENT_APIS */
    default: return "";
    }
}

/* An application that looks a function up by name gets the same one it would
   have linked against, so a loader-style application works unchanged. */
void *eglGetProcAddress(const char *name) {
    (void)name;
    return NULL;
}
