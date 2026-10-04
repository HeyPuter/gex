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

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "gexgl.h"
#include "gexgl-pixel.h"
#include "gexgl-plan.h"

/* ---------------------------------------------------------------- the ring */

/* GEXGL_MAGIC and GEXGL_SUBMIT are generated into gexgl.h from the Rust
   definitions, because writing them twice is writing them differently. */
#define RING_MAGIC GEXGL_MAGIC
#define GEX_GL_SUBMIT GEXGL_SUBMIT
#define HEADER_BYTES 32
#define CMD_CAPACITY (32u * 1024 * 1024)
#define REPLY_CAPACITY (32u * 1024 * 1024)

struct header {
    uint32_t magic;
    uint32_t capacity;
    uint32_t tail;
    uint32_t reply_capacity;
    uint32_t reply_len;
    uint32_t status;
    uint32_t failed_at;
    uint32_t protocol_version;
};

static int gl_fd = -1;
static struct header *ring;
static uint8_t *cmds;   /* the command area, just past the header */
static uint8_t *replies;
/* How much of the reply the caller has read, so several return values from
   one batch come back in order. */
static uint32_t reply_at;
static struct gexgl_pixel_store pack_store = {4, 0, 0, 0, 0, 0};
static struct gexgl_pixel_store unpack_store = {4, 0, 0, 0, 0, 0};

static int ring_open(void) {
    if (ring) {
        return 0;
    }
    gl_fd = open("/dev/gex-gl", O_RDWR);
    if (gl_fd < 0) {
        /* Said out loud. A silent failure here is indistinguishable, from the
           application's side, from a GL that simply reports nothing: every
           later glGetString answers from the empty string cache and the app
           selects a fallback renderer, many screens away from the cause. */
        fprintf(stderr, "gexgl: open(/dev/gex-gl) failed: %s\n",
                strerror(errno));
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
    ring->protocol_version = GEXGL_PROTOCOL_VERSION;
    cmds = (uint8_t *)p + HEADER_BYTES;
    replies = cmds + CMD_CAPACITY;
    return 0;
}

/* Which command last went into the ring, and how often each one was the last
 * thing before a submit.
 *
 * A submit is a round trip -- measured at ~0.3 ms here -- so what matters for
 * frame rate is not how many GL calls an application makes but how many of
 * them need an answer. es2gears spends 45% of its wall time in submits, a
 * hundred of them a frame, and nothing in the transport said which call was
 * asking. GEXGL_SUBMIT_TRACE=1 prints the tally at exit. */
#define GEXGL_MAX_TRACED_OPCODE 4096
static unsigned submit_trace_last_opcode;
static unsigned submit_trace_counts[GEXGL_MAX_TRACED_OPCODE];
static unsigned submit_trace_total;
static int submit_trace_enabled = -1;

static void gexgl_report_submit_trace(void) {
    if (!submit_trace_total) return;
    fprintf(stderr, "gexgl: %u submits, by the command that preceded them\n",
            submit_trace_total);
    /* Non-destructive: this is reported periodically as well as at exit,
       because a client the harness stops with a signal never reaches atexit
       and the tally would only ever be printed for programs that end by
       themselves. */
    unsigned ceiling = ~0u;
    for (int rank = 0; rank < 10; ++rank) {
        unsigned best = 0, best_op = 0;
        for (unsigned op = 0; op < GEXGL_MAX_TRACED_OPCODE; ++op)
            if (submit_trace_counts[op] > best &&
                submit_trace_counts[op] < ceiling) {
                best = submit_trace_counts[op];
                best_op = op;
            }
        if (!best) break;
        fprintf(stderr, "gexgl:   opcode %u: %u (%.1f%%)\n", best_op, best,
                100.0 * best / submit_trace_total);
        ceiling = best;
    }
}

static void gexgl_note_submit(void) {
    if (submit_trace_enabled < 0) {
        submit_trace_enabled = getenv("GEXGL_SUBMIT_TRACE") ? 1 : 0;
        if (submit_trace_enabled) atexit(gexgl_report_submit_trace);
    }
    if (!submit_trace_enabled) return;
    ++submit_trace_total;
    if (submit_trace_last_opcode < GEXGL_MAX_TRACED_OPCODE)
        ++submit_trace_counts[submit_trace_last_opcode];
    if (submit_trace_total % 5000u == 0) gexgl_report_submit_trace();
}

/* Hand everything recorded so far to the host and wait for it. */
static void flush(void) {
    if (!ring || ring->tail == 0) {
        reply_at = 0;
        return;
    }
    gexgl_note_submit();
    if (ioctl(gl_fd, GEX_GL_SUBMIT, ring) != 0) {
        /* Said out loud, because the alternative is losing GL commands in
           silence. A failed submit drops the whole queued batch, and the
           application is never told: object creation vanishes and the first
           sign is something far away and much later -- an attach of a name
           the host never saw, reported as an incomplete framebuffer. */
        static int said;
        if (!said) {
            said = 1;
            /* Named, with what to do about it. The commonest cause by far is
               a host module older than this library: the page's wasm is a
               build artifact and this library is shipped with the image, so
               they drift apart every time the protocol grows -- and what a
               person sees is a black window, with no reason to suspect a
               build. */
            const char *why =
                ring->status == 5u
                    ? "the host's GL module speaks a different protocol"
                      " version -- rebuild the page's wasm modules"
                : ring->status == 2u
                    ? "the host does not know one of these commands -- its GL"
                      " module is older than this library"
                : ring->status == 1u ? "the batch was truncated"
                : ring->status == 3u ? "an argument the host would not take"
                : ring->status == 4u ? "there is no GL context"
                : "no reason given";
            fprintf(stderr,
                    "gexgl: submit failed, discarding %u queued bytes -- GL"
                    " commands are being lost\n"
                    "gexgl: status %u: %s (this library speaks protocol %u)\n",
                    ring->tail, ring->status, why,
                    (unsigned)GEXGL_PROTOCOL_VERSION);
        }
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
    if (tail_bytes > SIZE_MAX - 3u || arg_words > (SIZE_MAX - 8u) / 4u)
        return NULL;
    size_t padded = (tail_bytes + 3u) & ~3u;
    size_t args_bytes = arg_words * 4u;
    if (padded > SIZE_MAX - 8u - args_bytes) return NULL;
    size_t len = 8u + args_bytes + padded;
    if (len > UINT32_MAX) return NULL;
    if (ring->tail > ring->capacity || len > ring->capacity - ring->tail) {
        flush();
        if (len > ring->capacity) {
            return NULL; /* one command larger than the whole buffer */
        }
    }
    uint32_t *at = (uint32_t *)(cmds + ring->tail);
    at[0] = opcode;
    at[1] = (uint32_t)len;
    ring->tail += (uint32_t)len;
    submit_trace_last_opcode = opcode;
    return at + 2;
}

static void put_blob(uint32_t *at, const void *data, size_t len) {
    if (len > UINT32_MAX) return;
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
    if (!ring || reply_at > ring->reply_len || ring->reply_len - reply_at < 4) {
        return 0;
    }
    uint32_t v;
    memcpy(&v, replies + reply_at, 4);
    reply_at += 4;
    return v;
}

static const uint8_t *reply_blob(uint32_t *len_out) {
    uint32_t len = reply_u32();
    uint32_t padded;
    if (!ring || reply_at > ring->reply_len ||
        len > ring->reply_len - reply_at || len > UINT32_MAX - 3u) {
        *len_out = 0;
        return NULL;
    }
    padded = (len + 3u) & ~3u;
    if (padded > ring->reply_len - reply_at) {
        *len_out = 0;
        return NULL;
    }
    const uint8_t *p = replies + reply_at;
    reply_at += padded;
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
void glDepthMask(unsigned char enabled) { CMD1(GEXGL_DEPTH_MASK, enabled); }
void glHint(unsigned target, unsigned mode) { CMD2(GEXGL_HINT, target, mode); }
void glColorMask(unsigned char r, unsigned char g, unsigned char b, unsigned char alpha) {
    CMD4(GEXGL_COLOR_MASK, r, g, b, alpha);
}
void glBlendEquation(unsigned mode) { CMD1(GEXGL_BLEND_EQUATION, mode); }
void glBlendFuncSeparate(unsigned srgb, unsigned drgb, unsigned sa, unsigned da) {
    CMD4(GEXGL_BLEND_FUNC_SEPARATE, srgb, drgb, sa, da);
}
void glPolygonOffset(float factor, float units) {
    CMD2(GEXGL_POLYGON_OFFSET, as_bits(factor), as_bits(units));
}
void glLineWidth(float width) { CMD1(GEXGL_LINE_WIDTH, as_bits(width)); }
void glClearStencil(int value) { CMD1(GEXGL_CLEAR_STENCIL, value); }
void glDepthRangef(float near_value, float far_value) {
    CMD2(GEXGL_DEPTH_RANGE, as_bits(near_value), as_bits(far_value));
}
void glClear(unsigned mask) { CMD1(GEXGL_CLEAR, mask); }
void glViewport(int x, int y, int w, int h) { CMD4(GEXGL_VIEWPORT, x, y, w, h); }
void glScissor(int x, int y, int w, int h) { CMD4(GEXGL_SCISSOR, x, y, w, h); }
void glEnable(unsigned cap) { CMD1(GEXGL_ENABLE, cap); }
void glDisable(unsigned cap) { CMD1(GEXGL_DISABLE, cap); }
void glBlendFunc(unsigned s, unsigned d) { CMD2(GEXGL_BLEND_FUNC, s, d); }
void glDepthFunc(unsigned f) { CMD1(GEXGL_DEPTH_FUNC, f); }
void glCullFace(unsigned m) { CMD1(GEXGL_CULL_FACE, m); }
void glFrontFace(unsigned m) { CMD1(GEXGL_FRONT_FACE, m); }
void glPixelStorei(unsigned p, int v) {
    (void)gexgl_set_pixel_store(&pack_store, &unpack_store, p, v);
    CMD2(GEXGL_PIXEL_STOREI, p, v);
}

void glFinish(void) {
    CMD0(GEXGL_FINISH);
    flush();
    (void)reply_u32();
}

void glFlush(void) { flush(); }

void glGenBuffers(int n, unsigned *out) {
    /* A negative count has to reach the host so GL can answer
     * GL_INVALID_VALUE; dropping it here made glGenBuffers(-1) succeed
     * silently, which KHR-GL32.buffer_objects.gen_buffers checks. */
    if (n < 0) { CMD1(GEXGL_GEN_BUFFERS, (uint32_t)n); return; }
    uint32_t *p = begin(GEXGL_GEN_BUFFERS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) {
        out[i] = take_name();
        p[1 + i] = out[i];
    }
}

static void gen_named_objects(unsigned opcode, int n, unsigned *out) {
    if (n < 0) {
        /* Let the host implementation record GL_INVALID_VALUE. */
        CMD1(opcode, n);
        return;
    }
    uint32_t *p = begin(opcode, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; ++i) {
        out[i] = take_name();
        p[1 + i] = out[i];
    }
}

static void delete_named_objects(unsigned opcode, int n, const unsigned *names) {
    if (n < 0) {
        CMD1(opcode, n);
        return;
    }
    uint32_t *p = begin(opcode, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; ++i) p[1 + i] = names[i];
}

void glGenQueries(int n, unsigned *ids) { gen_named_objects(GEXGL_GEN_QUERIES, n, ids); }
void glDeleteQueries(int n, const unsigned *ids) { delete_named_objects(GEXGL_DELETE_QUERIES, n, ids); }
unsigned char glIsQuery(unsigned id) {
    CMD1(GEXGL_IS_QUERY, id); flush(); return (unsigned char)reply_u32();
}

void glGenTransformFeedbacks(int n, unsigned *ids) {
    gen_named_objects(GEXGL_GEN_TRANSFORM_FEEDBACKS, n, ids);
}
void glDeleteTransformFeedbacks(int n, const unsigned *ids) {
    delete_named_objects(GEXGL_DELETE_TRANSFORM_FEEDBACKS, n, ids);
}
void glBindTransformFeedback(unsigned target, unsigned id) {
    CMD2(GEXGL_BIND_TRANSFORM_FEEDBACK, target, id);
}
unsigned char glIsTransformFeedback(unsigned id) {
    CMD1(GEXGL_IS_TRANSFORM_FEEDBACK, id); flush(); return (unsigned char)reply_u32();
}
void glPauseTransformFeedback(void) { CMD0(GEXGL_PAUSE_TRANSFORM_FEEDBACK); }
void glResumeTransformFeedback(void) { CMD0(GEXGL_RESUME_TRANSFORM_FEEDBACK); }

static void forget_buffer_object(unsigned name);

void glDeleteBuffers(int n, const unsigned *names) {
    if (n < 0) { CMD1(GEXGL_DELETE_BUFFERS, (uint32_t)n); return; }
    uint32_t *p = begin(GEXGL_DELETE_BUFFERS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) {
        p[1 + i] = names[i];
        forget_buffer_object(names[i]);
    }
}

struct buffer_target_state {
    unsigned target;
    unsigned name;
};
struct buffer_object_state {
    unsigned name;
    size_t size;
    void *map;
    size_t map_offset;
    size_t map_length;
    unsigned map_access;
    struct buffer_object_state *next;
};
static struct buffer_target_state buffer_targets[16];
static struct buffer_object_state *buffer_objects;

static struct buffer_target_state *buffer_target(unsigned target) {
    struct buffer_target_state *free_slot = NULL;
    for (size_t i = 0; i < sizeof(buffer_targets) / sizeof(buffer_targets[0]); i++) {
        if (buffer_targets[i].target == target) return &buffer_targets[i];
        if (!buffer_targets[i].target && !free_slot) free_slot = &buffer_targets[i];
    }
    if (free_slot) free_slot->target = target;
    return free_slot;
}

static unsigned bound_buffer(unsigned target) {
    struct buffer_target_state *s = buffer_target(target);
    return s ? s->name : 0;
}

static struct buffer_object_state *buffer_object(unsigned name, int create) {
    struct buffer_object_state *object = buffer_objects;
    while (object) {
        if (object->name == name) return object;
        object = object->next;
    }
    if (!create || !name) return NULL;
    object = calloc(1, sizeof(*object));
    if (!object) return NULL;
    object->name = name;
    object->next = buffer_objects;
    buffer_objects = object;
    return object;
}

static void forget_buffer_object(unsigned name) {
    for (size_t i = 0; i < sizeof(buffer_targets) / sizeof(buffer_targets[0]); ++i) {
        if (buffer_targets[i].name == name) buffer_targets[i].name = 0;
    }
    struct buffer_object_state **link = &buffer_objects;
    while (*link) {
        struct buffer_object_state *object = *link;
        if (object->name == name) {
            *link = object->next;
            free(object->map);
            free(object);
            return;
        }
        link = &object->next;
    }
}

void glBindBuffer(unsigned target, unsigned name) {
    struct buffer_target_state *s = buffer_target(target);
    if (s) s->name = name;
    if (name) (void)buffer_object(name, 1);
    CMD2(GEXGL_BIND_BUFFER, target, name);
}

void glBufferData(unsigned target, long size, const void *data, unsigned usage) {
    if (size < 0) return;
    size_t len = (size_t)size;
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 1);
    if (object) object->size = len;
    size_t blob_len = data ? len : 0;
    uint32_t *p = begin(GEXGL_BUFFER_DATA, 5, 4 + blob_len);
    if (!p) return;
    p[0] = target;
    p[1] = usage;
    p[2] = (uint32_t)(uint64_t)size;
    p[3] = (uint32_t)((uint64_t)size >> 32);
    p[4] = data != NULL;
    put_blob(p + 5, data, blob_len);
}

void glBufferSubData(unsigned target, long offset, long size, const void *data) {
    if (offset < 0 || size < 0 || (size && !data)) return;
    size_t len = (size_t)size;
    uint32_t *p = begin(GEXGL_BUFFER_SUB_DATA, 5, 4 + len);
    if (!p) return;
    p[0] = target;
    p[1] = (uint32_t)(uint64_t)offset;
    p[2] = (uint32_t)((uint64_t)offset >> 32);
    p[3] = (uint32_t)(uint64_t)size;
    p[4] = (uint32_t)((uint64_t)size >> 32);
    put_blob(p + 5, data, len);
}

/* Generated below with the rest of the GL 4.4 synchronous query surface.
   Mapping a readable buffer must use that transport query to populate the
   guest shadow before returning it to an unmodified desktop application. */
void glGetBufferSubData(unsigned target, long offset, long size, void *data);

/* Mapped host memory cannot be exposed to an emulated guest address space.
   Return a guest shadow allocation and upload its written bytes at unmap.
   This is the same coherency boundary a remote/virtual GL driver uses. */
void *glMapBufferRange(unsigned target, long offset, long length,
                       unsigned access) {
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    if (!object || offset < 0 || length < 0 || object->map ||
        (size_t)offset > object->size || (size_t)length > object->size - (size_t)offset)
        return NULL;
    object->map_length = (size_t)length;
    object->map_offset = (size_t)offset;
    object->map_access = access;
    object->map = calloc(1, object->map_length ? object->map_length : 1);
    /* The shadow must hold the buffer's bytes wherever the application may
       read them (GL_MAP_READ_BIT) or where unmap uploads bytes it did not
       write: without GL_MAP_INVALIDATE_{RANGE,BUFFER}_BIT the range keeps
       its old contents. With GL_MAP_FLUSH_EXPLICIT_BIT only flushed ranges
       are uploaded, so nothing else needs to be read. */
    unsigned keep = (access & 1u) ||
                    !(access & (0x4u | 0x8u | 0x10u)); /* INVALIDATE_RANGE/BUFFER, FLUSH_EXPLICIT */
    if (object->map && keep && object->map_length)
        glGetBufferSubData(target, offset, length, object->map);
    return object->map;
}

void *glMapBuffer(unsigned target, unsigned access) {
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    unsigned bits = access == 0x88B8u ? 1u : /* GL_READ_ONLY */
                    access == 0x88B9u ? 2u : 3u; /* WRITE/READ_WRITE */
    return object ? glMapBufferRange(target, 0, (long)object->size, bits) : NULL;
}

unsigned char glUnmapBuffer(unsigned target) {
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    if (!object || !object->map) return 0;
    /* With GL_MAP_FLUSH_EXPLICIT_BIT the application said which ranges it
       wrote (glFlushMappedBufferRange, already uploaded); uploading the
       whole shadow would overwrite the rest of the range with zeros --
       a ring buffer's other, in-flight, contents. */
    if ((object->map_access & 2u) && !(object->map_access & 0x10u))
        glBufferSubData(target, (long)object->map_offset, (long)object->map_length, object->map);
    free(object->map);
    object->map = NULL;
    object->map_offset = object->map_length = 0;
    return 1;
}

void glFlushMappedBufferRange(unsigned target, long offset, long length) {
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    if (!object || !object->map || offset < 0 || length < 0 ||
        (size_t)offset > object->map_length ||
        (size_t)length > object->map_length - (size_t)offset) return;
    glBufferSubData(target, (long)(object->map_offset + (size_t)offset), length,
                    (uint8_t *)object->map + offset);
}

void glBindBufferBase(unsigned target, unsigned index, unsigned buffer) {
    CMD3(GEXGL_BIND_BUFFER_BASE, target, index, buffer);
}

void glGenSamplers(int n, unsigned *out) {
    uint32_t *p = begin(GEXGL_GEN_SAMPLERS, 1 + (size_t)n, 0); if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) { out[i] = take_name(); p[1 + i] = out[i]; }
}
void glDeleteSamplers(int n, const unsigned *names) {
    uint32_t *p = begin(GEXGL_DELETE_SAMPLERS, 1 + (size_t)n, 0); if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) p[1 + i] = names[i];
}
void glBindSampler(unsigned unit, unsigned sampler) { CMD2(GEXGL_BIND_SAMPLER, unit, sampler); }
void glSamplerParameteri(unsigned sampler, unsigned pname, int value) {
    CMD3(GEXGL_SAMPLER_PARAMETER_I, sampler, pname, value);
}
void glSamplerParameterf(unsigned sampler, unsigned pname, float value) {
    CMD3(GEXGL_SAMPLER_PARAMETER_F, sampler, pname, as_bits(value));
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
    if (count < 0 || (count > 0 && !strings)) return;
    size_t total = 0;
    for (int i = 0; i < count; i++) {
        size_t n;
        if (!strings[i]) return;
        n = lengths && lengths[i] >= 0 ? (size_t)lengths[i] : strlen(strings[i]);
        if (!gexgl_size_add(total, n, &total)) return;
    }
    size_t tail;
    if (total > UINT32_MAX || !gexgl_size_add(4, total, &tail)) return;
    uint32_t *p = begin(GEXGL_SHADER_SOURCE, 1, tail);
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
void glDetachShader(unsigned p, unsigned s) { CMD2(GEXGL_DETACH_SHADER, p, s); }
void glLinkProgram(unsigned p) { CMD1(GEXGL_LINK_PROGRAM, p); }
void glUseProgram(unsigned p) { CMD1(GEXGL_USE_PROGRAM, p); }
void glDeleteProgram(unsigned p) { CMD1(GEXGL_DELETE_PROGRAM, p); }

void glBindAttribLocation(unsigned program, unsigned index, const char *name) {
    if (!name) return;
    size_t len = strlen(name);
    if (len > UINT32_MAX - 4u) return;
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
    /* Terminate whether or not the host sent anything. A caller passes an
       uninitialized buffer -- GL only promises to write one when there is a
       log -- so leaving it untouched on an empty reply means the application
       prints whatever was on its stack. SuperTuxKart does exactly that, and
       reported its shader link failures as binary noise. */
    if (out && max > 0) {
        if (p) memcpy(out, p, n);
        else n = 0;
        out[n] = 0;
    }
    if (len) *len = (int)n;
}

void glGetShaderInfoLog(unsigned s, int max, int *len, char *out) {
    get_log(GEXGL_GET_SHADER_INFO_LOG, s, max, len, out);
}

/* WebGPU's shader arithmetic has the highp ranges required by GLES 3.  This
   query describes the execution environment and does not require a command
   round trip.  In particular, GLES applications are allowed to call it as
   part of shader-source construction before compiling their first shader. */
void glGetShaderPrecisionFormat(unsigned shader_type, unsigned precision_type,
                                int range[2], int *precision) {
    (void)shader_type;
    if (range) {
        if (precision_type == 0x8DF0 || precision_type == 0x8DF1 ||
            precision_type == 0x8DF2) { /* LOW/MEDIUM/HIGH_INT */
            range[0] = 31;
            range[1] = 30;
        } else {
            range[0] = 127;
            range[1] = 127;
        }
    }
    if (precision) {
        if (precision_type == 0x8DF0 || precision_type == 0x8DF1 ||
            precision_type == 0x8DF2)
            *precision = 0;
        else
            *precision = 23;
    }
}

void glGetProgramInfoLog(unsigned p, int max, int *len, char *out) {
    get_log(GEXGL_GET_PROGRAM_INFO_LOG, p, max, len, out);
}

int glGetUniformLocation(unsigned program, const char *name) {
    if (!name) return -1;
    size_t len = strlen(name);
    if (len > UINT32_MAX - 4u) return -1;
    uint32_t *p = begin(GEXGL_GET_UNIFORM_LOCATION, 1, 4 + len);
    if (!p) return -1;
    p[0] = program;
    put_blob(p + 1, name, len);
    flush();
    return (int)reply_u32();
}

int glGetAttribLocation(unsigned program, const char *name) {
    if (!name) return -1;
    size_t len = strlen(name);
    if (len > UINT32_MAX - 4u) return -1;
    uint32_t *p = begin(GEXGL_GET_ATTRIB_LOCATION, 1, 4 + len);
    if (!p) return -1;
    p[0] = program;
    put_blob(p + 1, name, len);
    flush();
    return (int)reply_u32();
}

unsigned glGetUniformBlockIndex(unsigned program, const char *name) {
    if (!name) return 0xffffffffu;
    size_t len = strlen(name);
    if (len > UINT32_MAX - 4u) return 0xffffffffu;
    uint32_t *p = begin(GEXGL_GET_UNIFORM_BLOCK_INDEX, 1, 4 + len);
    if (!p) return 0xffffffffu;
    p[0] = program;
    put_blob(p + 1, name, len);
    flush();
    return reply_u32();
}

void glUniformBlockBinding(unsigned program, unsigned block_index,
                           unsigned binding) {
    CMD3(GEXGL_UNIFORM_BLOCK_BINDING, program, block_index, binding);
}

void glGetAttachedShaders(unsigned program, int max_count, int *count,
                          unsigned *out) {
    CMD2(GEXGL_GET_ATTACHED_SHADERS, program, max_count);
    flush();
    uint32_t n = reply_u32();
    if (n > (uint32_t)(max_count > 0 ? max_count : 0))
        n = max_count > 0 ? (uint32_t)max_count : 0;
    for (uint32_t i = 0; i < n; i++) out[i] = reply_u32();
    if (count) *count = (int)n;
}

static void get_uniform(uint32_t opcode, unsigned program, int location,
                        void *out) {
    CMD2(opcode, program, location);
    flush();
    uint32_t bytes = 0;
    const uint8_t *values = reply_blob(&bytes);
    if (out && values && bytes) memcpy(out, values, bytes);
}
void glGetUniformfv(unsigned program, int location, float *out) {
    get_uniform(GEXGL_GET_UNIFORM_FV, program, location, out);
}
void glGetUniformiv(unsigned program, int location, int *out) {
    get_uniform(GEXGL_GET_UNIFORM_IV, program, location, out);
}

void glUniform1i(int l, int v) { CMD2(GEXGL_UNIFORM_1I, l, v); }
void glUniform2i(int l, int x, int y) { CMD3(GEXGL_UNIFORM_2I, l, x, y); }
void glUniform3i(int l, int x, int y, int z) { CMD4(GEXGL_UNIFORM_3I, l, x, y, z); }
void glUniform4i(int l, int x, int y, int z, int w) {
    uint32_t *p = begin(GEXGL_UNIFORM_4I, 5, 0); if (!p) return;
    p[0] = (uint32_t)l; p[1] = (uint32_t)x; p[2] = (uint32_t)y;
    p[3] = (uint32_t)z; p[4] = (uint32_t)w;
}
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
    size_t len, tail;
    if (count < 0 || (count > 0 && !v) ||
        !gexgl_size_mul((size_t)count, 64, &len) ||
        !gexgl_size_add(4, len, &tail)) return;
    uint32_t *p = begin(GEXGL_UNIFORM_MATRIX4FV, 3, tail);
    if (!p) return;
    p[0] = (uint32_t)l;
    p[1] = (uint32_t)count;
    p[2] = transpose;
    put_blob(p + 3, v, len);
}

/* The offsets handed to glVertexAttribPointer. Only this side ever knew them:
   the host is told an offset into the bound buffer, not a guest pointer. */
#define GEXGL_MAX_VERTEX_ATTRIB_POINTERS 32
static void *vertex_attrib_pointers[GEXGL_MAX_VERTEX_ATTRIB_POINTERS];

static void get_active(uint32_t opcode, unsigned program, unsigned index,
                       int max, int *length, int *size, unsigned *type,
                       char *name) {
    CMD2(opcode, program, index);
    flush();
    int got_size = (int)reply_u32();
    unsigned got_type = reply_u32();
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    uint32_t copy = n;
    if (copy >= (uint32_t)(max > 0 ? max : 0)) copy = max > 0 ? (uint32_t)max - 1 : 0;
    if (name && max > 0) {
        if (p && copy) memcpy(name, p, copy);
        name[copy] = 0;
    }
    if (length) *length = (int)copy;
    if (size) *size = got_size;
    if (type) *type = got_type;
}

void glGetActiveUniform(unsigned program, unsigned index, int max, int *length,
                        int *size, unsigned *type, char *name) {
    get_active(GEXGL_GET_ACTIVE_UNIFORM, program, index, max, length, size, type, name);
}

void glGetActiveAttrib(unsigned program, unsigned index, int max, int *length,
                       int *size, unsigned *type, char *name) {
    get_active(GEXGL_GET_ACTIVE_ATTRIB, program, index, max, length, size, type, name);
}

/* Same shape as the two above: a name, a size and a type for one recorded
   varying.  Without it the symbol is unresolved, and a program that calls it
   jumps to zero -- which is how a whole conformance family dies at its second
   case rather than reporting a failure. */
void glGetTransformFeedbackVarying(unsigned program, unsigned index, int max,
                                   int *length, int *size, unsigned *type,
                                   char *name) {
    get_active(GEXGL_GET_TRANSFORM_FEEDBACK_VARYING, program, index, max,
               length, size, type, name);
}

void glGetShaderSource(unsigned shader, int max, int *length, char *source) {
    get_log(GEXGL_GET_SHADER_SOURCE, shader, max, length, source);
}

/* A name for one indexed thing: the same reply as get_active without the size
   and type. */
static void get_indexed_name(uint32_t opcode, unsigned program, unsigned index,
                             int max, int *length, char *name) {
    CMD2(opcode, program, index);
    flush();
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    if ((int)n > max - 1) n = max > 0 ? (uint32_t)max - 1 : 0;
    if (p && name && max > 0) {
        memcpy(name, p, n);
        name[n] = 0;
    }
    if (length) *length = (int)n;
}

void glGetActiveUniformName(unsigned program, unsigned index, int max,
                            int *length, char *name) {
    get_indexed_name(GEXGL_GET_ACTIVE_UNIFORM_NAME, program, index, max,
                     length, name);
}

void glGetActiveUniformBlockName(unsigned program, unsigned index, int max,
                                 int *length, char *name) {
    get_indexed_name(GEXGL_GET_ACTIVE_UNIFORM_BLOCK_NAME, program, index, max,
                     length, name);
}

/* However many integers the query yields; the host knows the count from the
   pname, so the reply carries it. */
static void reply_ints(int *params) {
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    if (params && p && n)
        memcpy(params, p, n - (n % 4));
}

void glGetActiveUniformBlockiv(unsigned program, unsigned index,
                               unsigned pname, int *params) {
    uint32_t *p = begin(GEXGL_GET_ACTIVE_UNIFORM_BLOCKIV, 3, 0);
    if (!p) return;
    p[0] = program; p[1] = index; p[2] = pname;
    flush();
    reply_ints(params);
}

void glGetActiveUniformsiv(unsigned program, int count,
                           const unsigned *indices, unsigned pname,
                           int *params) {
    size_t len, tail;
    if (count < 0 || (count > 0 && !indices) ||
        !gexgl_size_mul((size_t)count, 4, &len) ||
        !gexgl_size_add(4, len, &tail)) return;
    uint32_t *p = begin(GEXGL_GET_ACTIVE_UNIFORMSIV, 2, tail);
    if (!p) return;
    p[0] = program; p[1] = pname;
    put_blob(p + 2, indices, len);
    flush();
    reply_ints(params);
}

void glGetUniformIndices(unsigned program, int count,
                         const char *const *names, unsigned *indices) {
    size_t total = 0;
    if (count < 0 || (count > 0 && !names)) return;
    for (int i = 0; i < count; ++i) {
        if (!names[i]) return;
        if (!gexgl_size_add(total, strlen(names[i]) + 1, &total)) return;
    }
    size_t tail;
    if (!gexgl_size_add(4, total, &tail)) return;
    uint32_t *p = begin(GEXGL_GET_UNIFORM_INDICES, 2, tail);
    if (!p) return;
    p[0] = program; p[1] = (uint32_t)count;
    /* One blob of NUL-terminated names, which is how the guest holds them and
       what the host can walk without a second length array. */
    char *at = (char *)(p + 2) + 4;
    ((uint32_t *)(p + 2))[0] = (uint32_t)total;
    for (int i = 0; i < count; ++i) {
        size_t n = strlen(names[i]) + 1;
        memcpy(at, names[i], n);
        at += n;
    }
    flush();
    uint32_t n = 0;
    const uint8_t *r = reply_blob(&n);
    if (indices && r && n)
        memcpy(indices, r, n - (n % 4));
}

/* Answered from the guest's own state: the host never saw a guest pointer, so
   only this side can say what it was. */
void glGetBufferPointerv(unsigned target, unsigned pname, void **params) {
    if (!params) return;
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    *params = (pname == 0x88BDu /* GL_BUFFER_MAP_POINTER */ && object)
        ? object->map : NULL;
}

void glGetVertexAttribPointerv(unsigned index, unsigned pname, void **pointer) {
    if (!pointer) return;
    *pointer = (pname == 0x8645u /* GL_VERTEX_ATTRIB_ARRAY_POINTER */ &&
                index < GEXGL_MAX_VERTEX_ATTRIB_POINTERS)
        ? vertex_attrib_pointers[index] : NULL;
}

static void uniform_vector(uint32_t opcode, int location, int count,
                           const void *values, size_t components) {
    size_t elements, len, tail;
    if (count < 0 || (count > 0 && !values) ||
        !gexgl_size_mul((size_t)count, components, &elements) ||
        !gexgl_size_mul(elements, 4, &len) ||
        !gexgl_size_add(4, len, &tail)) return;
    uint32_t *p = begin(opcode, 2, tail); if (!p) return;
    p[0] = (uint32_t)location; p[1] = (uint32_t)count; put_blob(p + 2, values, len);
}
void glUniform1iv(int l,int n,const int*v){uniform_vector(GEXGL_UNIFORM_1IV,l,n,v,1);}
void glUniform1fv(int l,int n,const float*v){uniform_vector(GEXGL_UNIFORM_1FV,l,n,v,1);}
void glUniform2fv(int l,int n,const float*v){uniform_vector(GEXGL_UNIFORM_2FV,l,n,v,2);}
void glUniform3fv(int l,int n,const float*v){uniform_vector(GEXGL_UNIFORM_3FV,l,n,v,3);}
void glUniform4fv(int l,int n,const float*v){uniform_vector(GEXGL_UNIFORM_4FV,l,n,v,4);}
void glUniform2iv(int l,int n,const int*v){uniform_vector(GEXGL_UNIFORM_2IV,l,n,v,2);}
void glUniform3iv(int l,int n,const int*v){uniform_vector(GEXGL_UNIFORM_3IV,l,n,v,3);}
void glUniform4iv(int l,int n,const int*v){uniform_vector(GEXGL_UNIFORM_4IV,l,n,v,4);}
static void uniform_matrix(uint32_t opcode,int l,int n,unsigned char transpose,
                           const float*v,size_t components){
    size_t elements,len,tail;
    if(n<0||(n>0&&!v)||!gexgl_size_mul((size_t)n,components,&elements)||
       !gexgl_size_mul(elements,4,&len)||!gexgl_size_add(4,len,&tail))return;
    uint32_t*p=begin(opcode,3,tail);if(!p)return;
    p[0]=(uint32_t)l;p[1]=(uint32_t)n;p[2]=transpose;put_blob(p+3,v,len);
}
void glUniformMatrix2fv(int l,int n,unsigned char t,const float*v){uniform_matrix(GEXGL_UNIFORM_MATRIX2FV,l,n,t,v,4);}
void glUniformMatrix3fv(int l,int n,unsigned char t,const float*v){uniform_matrix(GEXGL_UNIFORM_MATRIX3FV,l,n,t,v,9);}

/* Client-side vertex arrays.
 *
 * `glVertexAttribPointer(..., verts)` with no GL_ARRAY_BUFFER bound points at
 * the application's own memory. GLES 2 and 3 and desktop compatibility all
 * allow it, and it is how every mesa-demos program, and most tutorial code,
 * feeds a triangle. The host cannot dereference a guest pointer, so this used
 * to travel as if it were a buffer offset: the comment beside it claimed the
 * case was "rejected loudly" and nothing rejected anything. es2tri drew one
 * triangle out of whatever the host found at address 0x7ffd..., which is to
 * say it drew nothing, with no error anywhere.
 *
 * Uploading is done entirely on this side, into one scratch buffer per
 * attribute, using commands the protocol already has. The alternative --
 * sending the vertices inline the way glDrawElements sends client indices --
 * needs a new wire shape and host support for it, to no advantage.
 *
 * The scratch copy mirrors the client array from element zero, so `first`,
 * the stride and the index values all keep their meaning and the draw goes
 * out unchanged. */
struct vertex_attrib_state {
    int size;
    unsigned type;
    unsigned char normalized;
    unsigned char integer;
    int stride;
    const void *pointer;
    unsigned divisor;
    unsigned scratch;      /* our upload buffer, 0 until first needed */
    unsigned char client;  /* set with no array buffer bound */
    unsigned char enabled;
};
static struct vertex_attrib_state vertex_attribs[GEXGL_MAX_VERTEX_ATTRIB_POINTERS];
static int vertex_attrib_client_count;

/* Bytes one attribute occupies, ignoring stride. */
static size_t attrib_element_bytes(int size, unsigned type) {
    size_t component;
    switch (type) {
    case 0x1400: case 0x1401: component = 1; break; /* GL_[UNSIGNED_]BYTE */
    case 0x1402: case 0x1403: component = 2; break; /* GL_[UNSIGNED_]SHORT */
    case 0x140B: component = 2; break;              /* GL_HALF_FLOAT */
    case 0x1404: case 0x1405: component = 4; break; /* GL_[UNSIGNED_]INT */
    case 0x1406: case 0x140C: component = 4; break; /* GL_FLOAT, GL_FIXED */
    case 0x140A: component = 8; break;              /* GL_DOUBLE */
    /* The packed types are one dword whatever `size` says. */
    case 0x8D9F: case 0x8368: case 0x8C3B: return 4;
    default: return 0;
    }
    if (size < 1 || size > 4) return 0;
    return component * (size_t)size;
}

void glEnableVertexAttribArray(unsigned i) {
    if (i < GEXGL_MAX_VERTEX_ATTRIB_POINTERS) vertex_attribs[i].enabled = 1;
    CMD1(GEXGL_ENABLE_VERTEX_ATTRIB_ARRAY, i);
}
void glDisableVertexAttribArray(unsigned i) {
    if (i < GEXGL_MAX_VERTEX_ATTRIB_POINTERS) vertex_attribs[i].enabled = 0;
    CMD1(GEXGL_DISABLE_VERTEX_ATTRIB_ARRAY, i);
}

static void gexgl_record_attrib(unsigned index, int size, unsigned type,
                                unsigned char normalized, unsigned char integer,
                                int stride, const void *pointer) {
    if (index >= GEXGL_MAX_VERTEX_ATTRIB_POINTERS) return;
    struct vertex_attrib_state *a = &vertex_attribs[index];
    const unsigned char was_client = a->client;
    a->size = size;
    a->type = type;
    a->normalized = normalized;
    a->integer = integer;
    a->stride = stride;
    a->pointer = pointer;
    /* GL_ARRAY_BUFFER. Which one is bound *now* is what decides, for good:
       binding a buffer afterwards does not retroactively capture the array. */
    a->client = bound_buffer(0x8892u) == 0 && pointer != NULL;
    if (a->client != was_client)
        vertex_attrib_client_count += a->client ? 1 : -1;
}

/* Copy every enabled client-side array into a buffer the host can read.
 *
 * `vertices` is how many elements the draw will reach for a divisor-0
 * attribute -- `first + count` for glDrawArrays, `max index + 1` for
 * glDrawElements. An instanced attribute is indexed by instance instead, so
 * it needs ceil(instances / divisor).
 *
 * Counting from element zero rather than from `first` is deliberate: the copy
 * then has the same layout as the original, so the draw command itself, its
 * `first`, and any index values need no adjustment. */
/* The fixed-function arrays (glVertexPointer and friends), kept like the
   generic attributes above: a pointer with no GL_ARRAY_BUFFER bound is client
   memory, uploaded just before each draw. Index 0 is the vertex array, 1 the
   colour array, 2 + n texture unit n's coordinates. */
#define GEXGL_FF_ARRAYS 6
struct ff_array_state {
    int size;
    unsigned type;
    int stride;
    const void *pointer;
    unsigned scratch;
    unsigned char client;
    unsigned char enabled;
};
static struct ff_array_state ff_arrays[GEXGL_FF_ARRAYS];
static int ff_client_count;
static unsigned client_active_texture;

static void gexgl_upload_ff_arrays(size_t vertices);

static void gexgl_upload_client_arrays(size_t vertices, int instances) {
    if (ff_client_count > 0) gexgl_upload_ff_arrays(vertices);
    if (vertex_attrib_client_count <= 0) return;
    const unsigned previous = bound_buffer(0x8892u); /* GL_ARRAY_BUFFER */
    for (unsigned i = 0; i < GEXGL_MAX_VERTEX_ATTRIB_POINTERS; ++i) {
        struct vertex_attrib_state *a = &vertex_attribs[i];
        if (!a->enabled || !a->client || !a->pointer) continue;
        const size_t element = attrib_element_bytes(a->size, a->type);
        if (!element) continue;
        const size_t stride = a->stride > 0 ? (size_t)a->stride : element;
        size_t needed = vertices;
        if (a->divisor) {
            const size_t n = instances > 0 ? (size_t)instances : 1;
            needed = (n + a->divisor - 1) / a->divisor;
        }
        if (!needed) continue;
        /* The last element needs only its own bytes, not a whole stride --
           an application is entitled to end its array exactly there, and
           reading a stride past it would fault on a tight allocation. */
        if (needed - 1 > (SIZE_MAX - element) / stride) continue;
        const size_t bytes = (needed - 1) * stride + element;
        if (!a->scratch) {
            unsigned name = 0;
            glGenBuffers(1, &name);
            if (!name) continue;
            a->scratch = name;
        }
        glBindBuffer(0x8892u, a->scratch);
        glBufferData(0x8892u, (long)bytes, a->pointer, 0x88E8u /* DYNAMIC_DRAW */);
        /* Re-point at the copy. This goes straight to the wire rather than
           through the public entry point, which would re-record the
           attribute as buffer-backed and lose the client pointer. */
        if (a->integer) {
            uint32_t *p = begin(GEXGL_VERTEX_ATTRIB_I_POINTER, 5, 0);
            if (p) { p[0]=i; p[1]=(uint32_t)a->size; p[2]=a->type;
                     p[3]=(uint32_t)a->stride; p[4]=0; }
        } else {
            uint32_t *p = begin(GEXGL_VERTEX_ATTRIB_POINTER, 6, 0);
            if (p) { p[0]=i; p[1]=(uint32_t)a->size; p[2]=a->type;
                     p[3]=a->normalized; p[4]=(uint32_t)a->stride; p[5]=0; }
        }
    }
    glBindBuffer(0x8892u, previous);
}

static void ff_pointer_command(unsigned slot, const struct ff_array_state *a, uint64_t offset) {
    static const uint32_t ops[3] = { GEXGL_VERTEX_POINTER, GEXGL_COLOR_POINTER, GEXGL_TEX_COORD_POINTER };
    uint32_t *p = begin(ops[slot < 2 ? slot : 2], 5, 0);
    if (!p) return;
    p[0] = (uint32_t)a->size; p[1] = a->type; p[2] = (uint32_t)a->stride;
    p[3] = (uint32_t)offset; p[4] = (uint32_t)(offset >> 32);
}

static void gexgl_upload_ff_arrays(size_t vertices) {
    if (!vertices) return;
    const unsigned previous = bound_buffer(0x8892u); /* GL_ARRAY_BUFFER */
    const unsigned unit = client_active_texture;
    for (unsigned i = 0; i < GEXGL_FF_ARRAYS; ++i) {
        struct ff_array_state *a = &ff_arrays[i];
        if (!a->enabled || !a->client || !a->pointer) continue;
        const size_t element = attrib_element_bytes(a->size, a->type);
        if (!element) continue;
        const size_t stride = a->stride > 0 ? (size_t)a->stride : element;
        if (vertices - 1 > (SIZE_MAX - element) / stride) continue;
        const size_t bytes = (vertices - 1) * stride + element;
        if (!a->scratch) {
            unsigned name = 0;
            glGenBuffers(1, &name);
            if (!name) continue;
            a->scratch = name;
        }
        glBindBuffer(0x8892u, a->scratch);
        glBufferData(0x8892u, (long)bytes, a->pointer, 0x88E8u /* DYNAMIC_DRAW */);
        if (i >= 2) CMD1(GEXGL_CLIENT_ACTIVE_TEXTURE, 0x84C0u + (i - 2));
        ff_pointer_command(i, a, 0);
    }
    CMD1(GEXGL_CLIENT_ACTIVE_TEXTURE, 0x84C0u + unit);
    glBindBuffer(0x8892u, previous);
}

static struct ff_array_state *ff_slot_for(unsigned array) {
    switch (array) {
    case 0x8074u: return &ff_arrays[0]; /* GL_VERTEX_ARRAY */
    case 0x8076u: return &ff_arrays[1]; /* GL_COLOR_ARRAY */
    case 0x8078u:                       /* GL_TEXTURE_COORD_ARRAY */
        return client_active_texture < GEXGL_FF_ARRAYS - 2 ? &ff_arrays[2 + client_active_texture] : NULL;
    default: return NULL;
    }
}

static void ff_record(unsigned slot, int size, unsigned type, int stride, const void *pointer) {
    struct ff_array_state *a = &ff_arrays[slot];
    const unsigned char was = a->client;
    a->size = size; a->type = type; a->stride = stride; a->pointer = pointer;
    a->client = bound_buffer(0x8892u) == 0 && pointer != NULL;
    if (a->client != was) ff_client_count += a->client ? 1 : -1;
    /* A buffer-backed array goes out now; a client one at the draw. */
    if (!a->client) ff_pointer_command(slot, a, (uint64_t)(uintptr_t)pointer);
}

void glEnableClientState(unsigned array) {
    struct ff_array_state *a = ff_slot_for(array);
    if (a) a->enabled = 1;
    CMD1(GEXGL_ENABLE_CLIENT_STATE, array);
}
void glDisableClientState(unsigned array) {
    struct ff_array_state *a = ff_slot_for(array);
    if (a) a->enabled = 0;
    CMD1(GEXGL_DISABLE_CLIENT_STATE, array);
}
void glVertexPointer(int size, unsigned type, int stride, const void *pointer) {
    ff_record(0, size, type, stride, pointer);
}
void glColorPointer(int size, unsigned type, int stride, const void *pointer) {
    ff_record(1, size, type, stride, pointer);
}
void glTexCoordPointer(int size, unsigned type, int stride, const void *pointer) {
    if (client_active_texture < GEXGL_FF_ARRAYS - 2)
        ff_record(2 + client_active_texture, size, type, stride, pointer);
}
void glClientActiveTexture(unsigned unit) {
    client_active_texture = unit >= 0x84C0u ? unit - 0x84C0u : 0;
    CMD1(GEXGL_CLIENT_ACTIVE_TEXTURE, unit);
}
/* glLockArraysEXT/glUnlockArraysEXT are a promise the arrays will not
   change between them; the uploads above already copy at every draw. */
void glLockArraysEXT(int first, int count) { (void)first; (void)count; }
void glUnlockArraysEXT(void) {}

void glVertexAttribPointer(unsigned index, int size, unsigned type,
                           unsigned char normalized, int stride, const void *offset) {
    /* Remembered here because glGetVertexAttribPointerv has to give it back
       and the host was never told a guest pointer. */
    if (index < GEXGL_MAX_VERTEX_ATTRIB_POINTERS)
        vertex_attrib_pointers[index] = (void *)(uintptr_t)offset;
    gexgl_record_attrib(index, size, type, normalized, 0, stride, offset);
    uint32_t *p = begin(GEXGL_VERTEX_ATTRIB_POINTER, 6, 0);
    if (!p) return;
    p[0] = index;
    p[1] = (uint32_t)size;
    p[2] = type;
    p[3] = normalized;
    p[4] = (uint32_t)stride;
    /* An offset into the bound buffer. When there is no buffer bound this is
       a pointer into guest memory instead, and the draw path uploads it just
       before the draw and re-points the attribute at the copy -- see
       gexgl_upload_client_arrays. */
    p[5] = (uint32_t)(uintptr_t)offset;
}

void glVertexAttribIPointer(unsigned index, int size, unsigned type,
                            int stride, const void *offset) {
    if (index < GEXGL_MAX_VERTEX_ATTRIB_POINTERS)
        vertex_attrib_pointers[index] = (void *)(uintptr_t)offset;
    gexgl_record_attrib(index, size, type, 0, 1, stride, offset);
    uint32_t *p = begin(GEXGL_VERTEX_ATTRIB_I_POINTER, 5, 0); if (!p) return;
    p[0]=index; p[1]=(uint32_t)size; p[2]=type; p[3]=(uint32_t)stride;
    p[4]=(uint32_t)(uintptr_t)offset;
}
void glVertexAttribDivisor(unsigned index, unsigned divisor) {
    if (index < GEXGL_MAX_VERTEX_ATTRIB_POINTERS)
        vertex_attribs[index].divisor = divisor;
    CMD2(GEXGL_VERTEX_ATTRIB_DIVISOR, index, divisor);
}
void glVertexAttrib4f(unsigned index, float x, float y, float z, float w) {
    uint32_t *p=begin(GEXGL_VERTEX_ATTRIB_4F,5,0); if(!p)return;
    p[0]=index; memcpy(&p[1],&x,4); memcpy(&p[2],&y,4);
    memcpy(&p[3],&z,4); memcpy(&p[4],&w,4);
}
void glVertexAttribI4i(unsigned index, int x, int y, int z, int w) {
    uint32_t *p=begin(GEXGL_VERTEX_ATTRIB_I4I,5,0); if(!p)return;
    p[0]=index; p[1]=(uint32_t)x; p[2]=(uint32_t)y;
    p[3]=(uint32_t)z; p[4]=(uint32_t)w;
}

void glGenVertexArrays(int n, unsigned *out) {
    uint32_t *p = begin(GEXGL_GEN_VERTEX_ARRAYS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) { out[i] = take_name(); p[1 + i] = out[i]; }
}
void glDeleteVertexArrays(int n, const unsigned *names) {
    uint32_t *p = begin(GEXGL_DELETE_VERTEX_ARRAYS, 1 + (size_t)n, 0);
    if (!p) return;
    p[0] = (uint32_t)n;
    for (int i = 0; i < n; i++) p[1 + i] = names[i];
}
void glBindVertexArray(unsigned name) { CMD1(GEXGL_BIND_VERTEX_ARRAY, name); }

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
void glTexParameterf(unsigned t, unsigned pname, float value) {
    uint32_t *p=begin(GEXGL_TEX_PARAMETER_F,3,0); if(!p)return;
    p[0]=t;p[1]=pname;memcpy(&p[2],&value,4);
}
void glTexParameteriv(unsigned t, unsigned pname, const int *value) {
    if (!value) return;
    size_t count = (pname == 0x1004u || pname == 0x8E46u) ? 4u : 1u;
    uint32_t *p = begin(GEXGL_TEX_PARAMETER_IV, 2 + count, 0);
    if (!p) return;
    p[0] = t;
    p[1] = pname;
    memcpy(&p[2], value, count * sizeof(*value));
}
void glTexParameterfv(unsigned t, unsigned pname, const float *value) {
    if (!value) return;
    size_t count = (pname == 0x1004u || pname == 0x8E46u) ? 4u : 1u;
    uint32_t *p = begin(GEXGL_TEX_PARAMETER_FV, 2 + count, 0);
    if (!p) return;
    p[0] = t;
    p[1] = pname;
    memcpy(&p[2], value, count * sizeof(*value));
}
void glSamplerParameteriv(unsigned sampler, unsigned pname, const int *value) {
    if (!value) return;
    size_t count = pname == 0x1004u ? 4u : 1u; /* GL_TEXTURE_BORDER_COLOR */
    uint32_t *p = begin(GEXGL_SAMPLER_PARAMETER_I, 2 + count, 0);
    if (!p) return;
    p[0] = sampler;
    p[1] = pname;
    memcpy(&p[2], value, count * sizeof(*value));
}
void glSamplerParameterfv(unsigned sampler, unsigned pname, const float *value) {
    if (!value) return;
    size_t count = pname == 0x1004u ? 4u : 1u; /* GL_TEXTURE_BORDER_COLOR */
    uint32_t *p = begin(GEXGL_SAMPLER_PARAMETER_F, 2 + count, 0);
    if (!p) return;
    p[0] = sampler;
    p[1] = pname;
    memcpy(&p[2], value, count * sizeof(*value));
}

static size_t pixel_transfer_bytes(unsigned format, unsigned type, int w, int h, int depth,
                                   const struct gexgl_pixel_store *store,
                                   size_t *offset) {
    struct gexgl_pixel_layout layout;
    if (!gexgl_compute_pixel_layout(format, type, w, h, depth, store, &layout)) {
        if (offset) *offset = 0;
        return 0;
    }
    if (offset) *offset = layout.offset;
    return layout.end;
}

void glTexImage2D(unsigned target, int level, int internal, int w, int h, int border,
                  unsigned format, unsigned type, const void *pixels) {
    int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */
    size_t len = pixels && !pbo ? pixel_transfer_bytes(format, type, w, h, -1,
                                                        &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEX_IMAGE_2D, 11, 4 + len);
    if (!p) return;
    p[0] = target; p[1] = (uint32_t)level; p[2] = (uint32_t)internal;
    p[3] = (uint32_t)w; p[4] = (uint32_t)h; p[5] = (uint32_t)border;
    p[6] = format; p[7] = type;
    p[8] = (uint32_t)pbo;
    p[9] = (uint32_t)(uintptr_t)pixels;
    p[10] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(p + 11, pbo ? NULL : pixels, len);
}

void glTexSubImage2D(unsigned target, int level, int x, int y, int w, int h,
                     unsigned format, unsigned type, const void *pixels) {
    int pbo = bound_buffer(0x88ECu) != 0;
    size_t len = pixels && !pbo ? pixel_transfer_bytes(format, type, w, h, -1,
                                                        &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEX_SUB_IMAGE_2D, 11, 4 + len);
    if (!p) return;
    p[0] = target; p[1] = (uint32_t)level; p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)w; p[5] = (uint32_t)h; p[6] = format; p[7] = type;
    p[8] = (uint32_t)pbo;
    p[9] = (uint32_t)(uintptr_t)pixels;
    p[10] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(p + 11, pbo ? NULL : pixels, len);
}

#define GEN_OBJECTS(fn, opcode) \
    void fn(int n, unsigned *out) { \
        if (n < 0) { CMD1(opcode, (uint32_t)n); return; } \
        uint32_t *p = begin(opcode, 1 + (size_t)n, 0); if (!p) return; \
        p[0] = (uint32_t)n; \
        for (int i = 0; i < n; i++) { out[i] = take_name(); p[1 + i] = out[i]; } \
    }
#define DELETE_OBJECTS(fn, opcode) \
    void fn(int n, const unsigned *names) { \
        if (n < 0) { CMD1(opcode, (uint32_t)n); return; } \
        uint32_t *p = begin(opcode, 1 + (size_t)n, 0); if (!p) return; \
        p[0] = (uint32_t)n; for (int i = 0; i < n; i++) p[1 + i] = names[i]; \
    }
GEN_OBJECTS(glGenFramebuffers, GEXGL_GEN_FRAMEBUFFERS)
DELETE_OBJECTS(glDeleteFramebuffers, GEXGL_DELETE_FRAMEBUFFERS)
GEN_OBJECTS(glGenRenderbuffers, GEXGL_GEN_RENDERBUFFERS)
DELETE_OBJECTS(glDeleteRenderbuffers, GEXGL_DELETE_RENDERBUFFERS)
void glBindFramebuffer(unsigned target, unsigned name) {
    CMD2(GEXGL_BIND_FRAMEBUFFER, target, name);
}
void glBindRenderbuffer(unsigned target, unsigned name) { CMD2(GEXGL_BIND_RENDERBUFFER, target, name); }
unsigned glCheckFramebufferStatus(unsigned target) {
    CMD1(GEXGL_CHECK_FRAMEBUFFER_STATUS, target);
    flush();
    unsigned status = reply_u32();
    if (status != 0x8CD5u)
        fprintf(stderr, "gex-gl: framebuffer status 0x%x for target 0x%x\n",
                status, target);
    return status;
}
void glFramebufferTexture2D(unsigned target, unsigned attachment, unsigned textarget,
                            unsigned texture, int level) {
    uint32_t *p = begin(GEXGL_FRAMEBUFFER_TEXTURE_2D, 5, 0); if (!p) return;
    p[0]=target; p[1]=attachment; p[2]=textarget; p[3]=texture; p[4]=(uint32_t)level;
}
void glFramebufferRenderbuffer(unsigned target, unsigned attachment,
                               unsigned renderbuffer_target, unsigned renderbuffer) {
    CMD4(GEXGL_FRAMEBUFFER_RENDERBUFFER, target, attachment, renderbuffer_target, renderbuffer);
}
void glRenderbufferStorage(unsigned target, unsigned internal, int w, int h) {
    CMD4(GEXGL_RENDERBUFFER_STORAGE, target, internal, w, h);
}
void glGenerateMipmap(unsigned target) { CMD1(GEXGL_GENERATE_MIPMAP, target); }

void glTexStorage2D(unsigned target, int levels, unsigned internal,
                    int width, int height) {
    uint32_t *p = begin(GEXGL_TEX_STORAGE_2D, 5, 0); if (!p) return;
    p[0]=target; p[1]=(uint32_t)levels; p[2]=internal;
    p[3]=(uint32_t)width; p[4]=(uint32_t)height;
}

void glTexStorage3D(unsigned target, int levels, unsigned internal,
                    int width, int height, int depth) {
    uint32_t *p = begin(GEXGL_TEX_STORAGE_3D, 6, 0); if (!p) return;
    p[0]=target; p[1]=(uint32_t)levels; p[2]=internal;
    p[3]=(uint32_t)width; p[4]=(uint32_t)height; p[5]=(uint32_t)depth;
}

void glTexImage3D(unsigned target, int level, int internal, int width,
                  int height, int depth, int border, unsigned format,
                  unsigned type, const void *data) {
    int pbo = bound_buffer(0x88ECu) != 0;
    size_t len = data && !pbo ? pixel_transfer_bytes(format, type, width, height, depth,
                                                      &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEX_IMAGE_3D, 12, 4 + len); if (!p) return;
    p[0]=target; p[1]=(uint32_t)level; p[2]=(uint32_t)internal;
    p[3]=(uint32_t)width; p[4]=(uint32_t)height; p[5]=(uint32_t)depth;
    p[6]=(uint32_t)border; p[7]=format; p[8]=type; p[9]=(uint32_t)pbo;
    p[10]=(uint32_t)(uintptr_t)data;
    p[11]=(uint32_t)((uint64_t)(uintptr_t)data >> 32);
    put_blob(p + 12, pbo ? NULL : data, len);
}

void glFramebufferTextureLayer(unsigned target, unsigned attachment,
                               unsigned texture, int level, int layer) {
    uint32_t *p = begin(GEXGL_FRAMEBUFFER_TEXTURE_LAYER, 5, 0); if (!p) return;
    p[0]=target; p[1]=attachment; p[2]=texture;
    p[3]=(uint32_t)level; p[4]=(uint32_t)layer;
}

void glDrawArrays(unsigned mode, int first, int count) {
    if (first >= 0 && count > 0)
        gexgl_upload_client_arrays((size_t)first + (size_t)count, 1);
    CMD3(GEXGL_DRAW_ARRAYS, mode, first, count);
}

/* The size of one index, or zero for a type glDrawElements does not take. */
static size_t index_element_bytes(unsigned type) {
    switch (type) {
    case 0x1401: return 1; /* GL_UNSIGNED_BYTE */
    case 0x1403: return 2; /* GL_UNSIGNED_SHORT */
    case 0x1405: return 4; /* GL_UNSIGNED_INT */
    default: return 0;
    }
}

/* The highest index a client-side glDrawElements will reach, plus one, so the
 * vertex arrays are uploaded as far as the draw actually reads. Scanning is
 * what the indices being on this side makes possible; a buffer-backed draw
 * cannot be measured here and does not need to be. */
static size_t gexgl_client_index_extent(const void *indices, int count,
                                        unsigned type) {
    if (!indices || count <= 0) return 0;
    size_t highest = 0;
    switch (type) {
    case 0x1401: { /* GL_UNSIGNED_BYTE */
        const unsigned char *v = indices;
        for (int i = 0; i < count; ++i) if (v[i] > highest) highest = v[i];
        break;
    }
    case 0x1403: { /* GL_UNSIGNED_SHORT */
        const unsigned short *v = indices;
        for (int i = 0; i < count; ++i) if (v[i] > highest) highest = v[i];
        break;
    }
    case 0x1405: { /* GL_UNSIGNED_INT */
        const unsigned *v = indices;
        for (int i = 0; i < count; ++i) if (v[i] > highest) highest = v[i];
        break;
    }
    default: return 0;
    }
    return highest + 1;
}

void glDrawElements(unsigned mode, int count, unsigned type, const void *offset) {
    /* With no element array buffer bound, `offset` is a pointer into the
     * client's own memory and means nothing on the host -- sending it as an
     * offset had the host dereference its own address space. The indices
     * travel with the command instead. */
    const int client = bound_buffer(0x8893u) == 0; /* GL_ELEMENT_ARRAY_BUFFER */
    const size_t element = index_element_bytes(type);
    /* Client vertex arrays are uploaded as far as these indices reach. With
       the indices in a buffer there is nothing to scan, so the whole array is
       out of reach and the draw is left to the host as before. */
    if ((vertex_attrib_client_count > 0 || ff_client_count > 0) && client)
        gexgl_upload_client_arrays(
            gexgl_client_index_extent(offset, count, type), 1);
    size_t blob_len = 0;
    if (client && offset && count > 0 && element &&
        (size_t)count <= SIZE_MAX / element)
        blob_len = (size_t)count * element;
    if (blob_len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_DRAW_ELEMENTS, 5, blob_len ? 4 + blob_len : 0);
    if (!p) return;
    p[0] = mode; p[1] = (uint32_t)count; p[2] = type;
    p[3] = (uint32_t)(uintptr_t)offset;
    p[4] = (uint32_t)(blob_len != 0);
    if (blob_len)
        put_blob(p + 5, offset, blob_len);
}

void glDrawBuffers(int count, const unsigned *buffers) {
    uint32_t *p=begin(GEXGL_DRAW_BUFFERS,1+(size_t)(count>0?count:0),0); if(!p)return;
    p[0]=(uint32_t)count;
    for(int i=0;i<count;i++)p[1+i]=buffers[i];
}
void glDrawArraysInstanced(unsigned mode, int first, int count, int instances) {
    if (first >= 0 && count > 0)
        gexgl_upload_client_arrays((size_t)first + (size_t)count, instances);
    CMD4(GEXGL_DRAW_ARRAYS_INSTANCED,mode,first,count,instances);
}
void glDrawElementsInstanced(unsigned mode, int count, unsigned type,
                             const void *offset, int instances) {
    uint32_t *p=begin(GEXGL_DRAW_ELEMENTS_INSTANCED,5,0); if(!p)return;
    p[0]=mode;p[1]=(uint32_t)count;p[2]=type;
    p[3]=(uint32_t)(uintptr_t)offset;p[4]=(uint32_t)instances;
}
void glBlitFramebuffer(int sx0,int sy0,int sx1,int sy1,
                       int dx0,int dy0,int dx1,int dy1,
                       unsigned mask,unsigned filter) {
    uint32_t *p=begin(GEXGL_BLIT_FRAMEBUFFER,10,0); if(!p)return;
    p[0]=(uint32_t)sx0;p[1]=(uint32_t)sy0;p[2]=(uint32_t)sx1;p[3]=(uint32_t)sy1;
    p[4]=(uint32_t)dx0;p[5]=(uint32_t)dy0;p[6]=(uint32_t)dx1;p[7]=(uint32_t)dy1;
    p[8]=mask;p[9]=filter;
}
void glBlendColor(float r,float g,float b,float a) {
    uint32_t *p=begin(GEXGL_BLEND_COLOR,4,0); if(!p)return;
    memcpy(&p[0],&r,4);memcpy(&p[1],&g,4);memcpy(&p[2],&b,4);memcpy(&p[3],&a,4);
}
void glBlendEquationSeparate(unsigned rgb,unsigned alpha) {
    CMD2(GEXGL_BLEND_EQUATION_SEPARATE,rgb,alpha);
}
void glClearBufferfv(unsigned buffer,int drawbuffer,const float *value) {
    uint32_t *p=begin(GEXGL_CLEAR_BUFFER_FV,6,0); if(!p)return;
    p[0]=buffer;p[1]=(uint32_t)drawbuffer;
    int count=buffer==0x1800u?4:1;
    for(int i=0;i<4;i++){float v=i<count&&value?value[i]:0.0f;memcpy(&p[2+i],&v,4);}
}
void glClearBufferiv(unsigned buffer,int drawbuffer,const int *value) {
    uint32_t *p=begin(GEXGL_CLEAR_BUFFER_IV,6,0); if(!p)return;
    p[0]=buffer;p[1]=(uint32_t)drawbuffer;
    int count=buffer==0x1800u?4:1;
    for(int i=0;i<4;i++)p[2+i]=(uint32_t)(i<count&&value?value[i]:0);
}
void glClearBufferuiv(unsigned buffer,int drawbuffer,const unsigned *value) {
    uint32_t *p=begin(GEXGL_CLEAR_BUFFER_UIV,6,0); if(!p)return;
    p[0]=buffer;p[1]=(uint32_t)drawbuffer;
    int count=buffer==0x1800u?4:1;
    for(int i=0;i<4;i++)p[2+i]=i<count&&value?value[i]:0;
}
void glClearBufferfi(unsigned buffer,int drawbuffer,float depth,int stencil) {
    uint32_t *p=begin(GEXGL_CLEAR_BUFFER_FI,4,0); if(!p)return;
    p[0]=buffer;p[1]=(uint32_t)drawbuffer;memcpy(&p[2],&depth,4);p[3]=(uint32_t)stencil;
}
void glStencilFunc(unsigned func,int ref,unsigned mask){CMD3(GEXGL_STENCIL_FUNC,func,ref,mask);}
void glStencilFuncSeparate(unsigned face,unsigned func,int ref,unsigned mask){CMD4(GEXGL_STENCIL_FUNC_SEPARATE,face,func,ref,mask);}
void glStencilMask(unsigned mask){CMD1(GEXGL_STENCIL_MASK,mask);}
void glStencilMaskSeparate(unsigned face,unsigned mask){CMD2(GEXGL_STENCIL_MASK_SEPARATE,face,mask);}
void glStencilOp(unsigned fail,unsigned zfail,unsigned zpass){CMD3(GEXGL_STENCIL_OP,fail,zfail,zpass);}
void glStencilOpSeparate(unsigned face,unsigned fail,unsigned zfail,unsigned zpass){CMD4(GEXGL_STENCIL_OP_SEPARATE,face,fail,zfail,zpass);}

void glReadPixels(int x, int y, int w, int h, unsigned format, unsigned type, void *out) {
    int pbo = bound_buffer(0x88EBu) != 0; /* GL_PIXEL_PACK_BUFFER */
    uint32_t *p = begin(GEXGL_READ_PIXELS, 9, 0);
    if (!p) return;
    p[0] = (uint32_t)x; p[1] = (uint32_t)y; p[2] = (uint32_t)w; p[3] = (uint32_t)h;
    p[4] = format; p[5] = type; p[6] = (uint32_t)pbo;
    p[7] = (uint32_t)(uintptr_t)out;
    p[8] = (uint32_t)((uint64_t)(uintptr_t)out >> 32);
    flush();
    if (pbo) return;
    struct gexgl_pixel_layout layout;
    int valid = gexgl_compute_pixel_layout(format, type, w, h, -1,
                                            &pack_store, &layout);
    uint32_t wire_offset = reply_u32();
    uint32_t n = 0;
    const uint8_t *px = reply_blob(&n);
    if (px && out && valid && wire_offset == layout.offset &&
        wire_offset <= layout.end && n == layout.end - wire_offset) {
        size_t active = 0;
        size_t group = gexgl_pixel_group_bytes(format, type);
        if (!group || !gexgl_size_mul((size_t)(w > 0 ? w : 0), group, &active))
            return;
        for (int row = 0; row < h; ++row) {
            size_t row_at;
            if (!gexgl_size_mul((size_t)row, layout.row_stride, &row_at) ||
                row_at > n || active > n - row_at) return;
            memcpy((uint8_t *)out + layout.offset + row_at, px + row_at, active);
        }
    }
}

/* Errors this library raises itself.
 *
 * A few entry points are answered entirely in the guest -- sync objects most
 * of all, which are identity and state with no host object behind them -- and
 * those still have to be able to fail the way GL says. The host's error queue
 * cannot carry them, because the host never saw the call. So one slot is kept
 * here and glGetError reports it first.
 *
 * One slot, not a queue: GL leaves the order of multiple recorded errors
 * unspecified and requires only that each be reported once, and the first
 * error is the one worth keeping -- a later call's error is usually a
 * consequence of the first. */
static _Atomic unsigned pending_error;

static void gexgl_set_error(unsigned error) {
    unsigned none = 0;
    atomic_compare_exchange_strong_explicit(&pending_error, &none, error,
                                            memory_order_relaxed,
                                            memory_order_relaxed);
}

unsigned glGetError(void) {
    const unsigned local =
        atomic_exchange_explicit(&pending_error, 0u, memory_order_relaxed);
    if (local) return local;
    CMD0(GEXGL_GET_ERROR);
    flush();
    return reply_u32();
}

/* WebGPU device loss is surfaced by the host transport itself; while the
   context is alive there is no pending GL reset. CTS calls this after every
   case when terminate-on-device-lost is enabled, including GL 4.4 cases. */
unsigned glGetGraphicsResetStatus(void) { return 0; /* GL_NO_ERROR */ }

/* The strings are asked for once and never change, so they are kept rather
   than fetched again -- an application that calls glGetString in a loop is
   not unusual. */
static char *cached_strings[8];
/* Selected by eglBindAPI. Declared here because desktop identity queries are
 * GL entry points even though EGL owns the selection. */
static unsigned egl_api = 0x30A0; /* EGL_OPENGL_ES_API */
/* Whether CONTEXT_CREATE has been sent. A process gets one host context, and
   an application may create several EGLContexts against it. */
static int host_context_made;
/* How many contexts the application believes it has. */
static int egl_contexts_live;

/* Defined below, next to the list it builds. */
static const char *gexgl_extension_string(void);

const unsigned char *glGetString(unsigned name) {
    /* GL_SHADING_LANGUAGE_VERSION is asked of the host like every other
       string. It used to be answered here with a fixed "4.40 ..." string,
       which was three things wrong at once: it named a backend that is gone,
       it claimed a language version the context does not have -- the host
       reports 3.30 -- and an application that believes it writes
       `#version 440` shaders that cannot compile. glxinfo prints it next to
       the real GL version, which is where the disagreement showed. */
    if (name == 0x1F03u) { /* GL_EXTENSIONS */
        return (const unsigned char *)gexgl_extension_string();
    }
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

/* The extensions this library can honour, generated from the Khronos registry
   and this object's own exported symbols. */
#include "gl-extensions-generated.inc"

/* Extensions answered here rather than by the host, so they are claimed
   whatever the host reports. */
static const char *const gexgl_local_extensions[] = {
    /* CTS requests a lose-context-on-reset context and consequently
       calls glGetGraphicsResetStatus after every 4.4 case. */
    "GL_KHR_robustness",
    /* Expose extension identity only after the complete core command
       family and its element-offset transport are implemented. The
       Khronos shared GL/ES test keys desktop support off this ARB
       token even though the functions are core since OpenGL 3.2. */
    "GL_ARB_draw_elements_base_vertex",
};

static int gexgl_extension_name_compare(const void *left, const void *right) {
    return strcmp((const char *)left, *(const char *const *)right);
}

/* Whether the guest may repeat a host extension.

   An extension is a promise about entry points as well as about behaviour,
   and this library exports a fixed set of functions, so repeating one whose
   commands are missing would turn an application's graceful fallback into a
   null call. The generated table holds exactly those whose registry-required
   commands are all exported here; anything else -- including a name the
   registry does not describe -- is withheld. */
static int gexgl_extension_claimable(const char *name) {
    return bsearch(name, gexgl_claimable_extensions,
                   sizeof(gexgl_claimable_extensions) /
                       sizeof(gexgl_claimable_extensions[0]),
                   sizeof(gexgl_claimable_extensions[0]),
                   gexgl_extension_name_compare) != NULL;
}

/* GL_NUM_EXTENSIONS straight from the host.

   Not glGetIntegerv: that one answers this pname from the list built below,
   which is what asks for it. */
static int gexgl_host_extension_count(void) {
    CMD1(GEXGL_GET_INTEGERV, 0x821Du); /* GL_NUM_EXTENSIONS */
    flush();
    uint32_t bytes = 0;
    const uint8_t *values = reply_blob(&bytes);
    int count = 0;
    if (values && bytes >= sizeof(count)) memcpy(&count, values, sizeof(count));
    return count > 0 ? count : 0;
}

/* The filtered list, built once.

   Before this the guest reported three extensions no matter what the host
   supported, so every driver capability was invisible to applications: an
   extension nothing can detect is an extension nothing can use. The host's
   list is the real one; this filters it and prepends the few answered here. */
static char **extension_names;
static int extension_count = -1;
static char *extension_string;

static void gexgl_build_extensions(void) {
    if (extension_count >= 0) return;
    const int local = (int)(sizeof(gexgl_local_extensions) /
                            sizeof(gexgl_local_extensions[0]));
    const int host = gexgl_host_extension_count();
    /* Counted before it is filled and never grown, so the list cannot
       outrun its allocation even if the host answers inconsistently. */
    char **names = malloc((size_t)(local + host) * sizeof(*names));
    if (!names) { extension_count = 0; return; }
    int n = 0;
    for (int i = 0; i < local; ++i) names[n++] = (char *)gexgl_local_extensions[i];
    for (int i = 0; i < host; ++i) {
        CMD2(GEXGL_GET_STRING_I, 0x1F03u, (unsigned)i);
        flush();
        uint32_t bytes = 0;
        const uint8_t *value = reply_blob(&bytes);
        if (!value || !bytes) continue;
        char *name = malloc(bytes + 1);
        if (!name) continue;
        memcpy(name, value, bytes);
        name[bytes] = 0;
        int duplicate = 0;
        for (int j = 0; j < local; ++j)
            if (strcmp(name, gexgl_local_extensions[j]) == 0) duplicate = 1;
        if (duplicate || !gexgl_extension_claimable(name)) {
            /* Worth being able to see: a capability added to the driver shows
               up in the host's list first and only reaches applications once
               this library exports the commands it needs, so an extension
               that seems not to have landed is usually sitting right here. */
            if (!duplicate && getenv("GEXGL_EXTENSION_TRACE"))
                fprintf(stderr, "gexgl: withheld %s\n", name);
            free(name);
            continue;
        }
        names[n++] = name;
    }
    extension_names = names;
    extension_count = n;
    if (getenv("GEXGL_EXTENSION_TRACE"))
        fprintf(stderr, "gexgl: %d of %d host extensions claimed\n",
                n - local, host);
}

/* The space-separated form. Core profiles make glGetString(GL_EXTENSIONS) an
   error, but ES contexts have no glGetStringi before 3.0 and applications
   still reach for this one, so it has to say the same thing as the indexed
   list rather than whatever the host would have returned unfiltered. */
static const char *gexgl_extension_string(void) {
    gexgl_build_extensions();
    if (extension_string) return extension_string;
    size_t bytes = 1;
    for (int i = 0; i < extension_count; ++i)
        bytes += strlen(extension_names[i]) + 1;
    char *joined = malloc(bytes);
    if (!joined) return "";
    joined[0] = 0;
    size_t at = 0;
    for (int i = 0; i < extension_count; ++i) {
        if (i) joined[at++] = ' ';
        const size_t length = strlen(extension_names[i]);
        memcpy(joined + at, extension_names[i], length);
        at += length;
    }
    joined[at] = 0;
    extension_string = joined;
    return extension_string;
}

const unsigned char *glGetStringi(unsigned name, unsigned index) {
    if (name == 0x1F03u) { /* GL_EXTENSIONS */
        gexgl_build_extensions();
        return (int)index < extension_count
            ? (const unsigned char *)extension_names[index]
            : (const unsigned char *)"";
    }
    CMD2(GEXGL_GET_STRING_I, name, index);
    flush();
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    char *s = malloc(n + 1);
    if (!s) return (const unsigned char *)"";
    if (p) memcpy(s, p, n);
    s[n] = 0;
    return (const unsigned char *)s;
}

/* The numeric form of whatever GL_VERSION the host reported.

   GL_VERSION is "<major>.<minor>..." for desktop and "OpenGL ES <major>.<minor>..."
   for ES.  Deriving the integers from that string rather than from eglBindAPI
   is what keeps the two forms consistent: an engine that checks
   GL_MAJOR_VERSION and one that parses the string must reach the same
   conclusion, and neither should be told more than the host can serve. */
static void host_gl_version(int *major, int *minor) {
    const char *v = cached_strings[0x1F02u & 7u];
    *major = 0;
    *minor = 0;
    if (!v) return;
    while (*v && (*v < '0' || *v > '9')) v++;
    int m = 0;
    while (*v >= '0' && *v <= '9') m = m * 10 + (*v++ - '0');
    int n = 0;
    if (*v == '.') {
        v++;
        while (*v >= '0' && *v <= '9') n = n * 10 + (*v++ - '0');
    }
    *major = m;
    *minor = n;
}

static size_t gexgl_state_query_components(unsigned pname);

void glGetIntegerv(unsigned pname, int *out) {
    if (pname == 0x821Bu || pname == 0x821Cu) { /* GL_MAJOR/MINOR_VERSION */
        int major = 0, minor = 0;
        host_gl_version(&major, &minor);
        if (out) out[0] = pname == 0x821Bu ? major : minor;
        return;
    }
    if (pname == 0x821Du) { /* GL_NUM_EXTENSIONS */
        gexgl_build_extensions();
        if (out) out[0] = extension_count;
        return;
    }
    if (pname == 0x821Eu) { /* GL_CONTEXT_FLAGS */
        if (out) out[0] = 0;
        return;
    }
    if (pname == 0x9126u) { /* GL_CONTEXT_PROFILE_MASK */
        if (out) out[0] = egl_api == 0x30A2u ? 1 : 0; /* GL_CONTEXT_CORE_PROFILE_BIT */
        return;
    }
    /* Sized from the pname before the request goes out, for two reasons.
     * gexgl_state_query_components answers three pnames with a nested
     * glGetIntegerv, which would otherwise flush this command early and read
     * its own reply back as this one's; and copying whatever the host sent
     * writes past the caller's array whenever the two disagree, which is an
     * out-of-bounds write into guest memory rather than a wrong value. */
    const size_t expected = gexgl_state_query_components(pname) * sizeof(*out);
    CMD1(GEXGL_GET_INTEGERV, pname);
    flush();
    uint32_t bytes = 0;
    const uint8_t *values = reply_blob(&bytes);
    if (out && values && expected && bytes == expected)
        memcpy(out, values, expected);
}

static size_t gexgl_state_query_components(unsigned pname) {
    switch (pname) {
    case 0x0BA2u: /* GL_VIEWPORT */
    case 0x0C10u: /* GL_SCISSOR_BOX */
    case 0x0C23u: /* GL_COLOR_WRITEMASK */
    case 0x0C22u: /* GL_COLOR_CLEAR_VALUE */
    case 0x8005u: /* GL_BLEND_COLOR */
        return 4;
    case 0x91BEu: /* GL_MAX_COMPUTE_WORK_GROUP_COUNT */
    case 0x91BFu: /* GL_MAX_COMPUTE_WORK_GROUP_SIZE */
        return 3;
    case 0x0B70u: /* GL_DEPTH_RANGE */
    case 0x0D3Au: /* GL_MAX_VIEWPORT_DIMS */
    case 0x846Du: /* GL_ALIASED_POINT_SIZE_RANGE */
    case 0x846Eu: /* GL_ALIASED_LINE_WIDTH_RANGE */
    case 0x0B12u: /* GL_POINT_SIZE_RANGE */
    case 0x0B22u: /* GL_SMOOTH_LINE_WIDTH_RANGE */
    case 0x825Du: /* GL_VIEWPORT_BOUNDS_RANGE */
    case 0x0B40u: /* GL_POLYGON_MODE */
        return 2;
    case 0x86A3u: { /* GL_COMPRESSED_TEXTURE_FORMATS */
        int count = 0; glGetIntegerv(0x86A2u, &count); /* GL_NUM_COMPRESSED_TEXTURE_FORMATS */
        return count > 0 && count <= 4096 ? (size_t)count : 0;
    }
    case 0x87FFu: { /* GL_PROGRAM_BINARY_FORMATS */
        int count = 0; glGetIntegerv(0x87FEu, &count); /* GL_NUM_PROGRAM_BINARY_FORMATS */
        return count > 0 && count <= 4096 ? (size_t)count : 0;
    }
    case 0x8DF8u: { /* GL_SHADER_BINARY_FORMATS */
        int count = 0; glGetIntegerv(0x8DF9u, &count); /* GL_NUM_SHADER_BINARY_FORMATS */
        return count > 0 && count <= 4096 ? (size_t)count : 0;
    }
    /* The fixed-function state, matrices among them (gl-pname-counts.h). */
    default: return gexgl_pname_count(pname);
    }
}

/* GL 3.2 sync objects. The browser command stream is ordered and copies every
 * upload before an ioctl returns, while presentation submits commands in that
 * same order. A guest fence therefore needs identity and API-visible state,
 * but no second host-side fence object: by the time the guest can observe it,
 * every earlier command is already owned by the renderer. Keeping this local
 * also avoids a synchronous browser round trip in STK's per-frame fence path. */
#define SYNC_SLOTS 4096u
static _Atomic uintptr_t sync_next;
static _Atomic uintptr_t sync_slots[SYNC_SLOTS];

static int sync_valid(uintptr_t handle) {
    if (!handle) return 0;
    size_t slot = (handle - 1) % SYNC_SLOTS;
    return atomic_load_explicit(&sync_slots[slot], memory_order_relaxed) == handle;
}

void *glFenceSync(unsigned condition, unsigned flags) {
    /* GL_SYNC_GPU_COMMANDS_COMPLETE, and flags must be zero. Returning NULL
     * was not enough: GL requires the error too, and a caller that checks
     * glGetError rather than the handle saw success. */
    if (condition != 0x9117u) {
        gexgl_set_error(0x0500u); /* GL_INVALID_ENUM */
        return NULL;
    }
    if (flags != 0) {
        gexgl_set_error(0x0501u); /* GL_INVALID_VALUE */
        return NULL;
    }
    uintptr_t serial = atomic_fetch_add_explicit(&sync_next, 1, memory_order_relaxed);
    size_t start = serial % SYNC_SLOTS;
    uintptr_t generation = serial / SYNC_SLOTS + 1;
    for (size_t offset = 0; offset < SYNC_SLOTS; offset++) {
        size_t slot = (start + offset) % SYNC_SLOTS;
        uintptr_t handle = generation * SYNC_SLOTS + slot + 1;
        uintptr_t empty = 0;
        if (atomic_compare_exchange_strong_explicit(
                &sync_slots[slot], &empty, handle,
                memory_order_relaxed, memory_order_relaxed))
            return (void *)handle;
    }
    return NULL;
}

void glDeleteSync(void *sync) {
    uintptr_t handle = (uintptr_t)sync;
    /* Zero is silently ignored; anything else that is not a sync object is an
     * error. */
    if (!handle) return;
    if (!sync_valid(handle)) {
        gexgl_set_error(0x0501u); /* GL_INVALID_VALUE */
        return;
    }
    size_t slot = (handle - 1) % SYNC_SLOTS;
    atomic_compare_exchange_strong_explicit(
        &sync_slots[slot], &handle, 0,
        memory_order_relaxed, memory_order_relaxed);
}

unsigned char glIsSync(void *sync) { return sync_valid((uintptr_t)sync); }

/* The errors below are as much a part of these entry points as the values
 * are: an object implemented in the guest still has to reject what GL says to
 * reject, and the host cannot do it for calls it never receives. */
unsigned glClientWaitSync(void *sync, unsigned flags, uint64_t timeout) {
    (void)timeout;
    if (!sync_valid((uintptr_t)sync)) {
        gexgl_set_error(0x0501u); /* GL_INVALID_VALUE */
        return 0x911Du; /* GL_WAIT_FAILED */
    }
    if (flags & ~0x00000001u) { /* only GL_SYNC_FLUSH_COMMANDS_BIT */
        gexgl_set_error(0x0501u);
        return 0x911Du;
    }
    return 0x911Au; /* GL_ALREADY_SIGNALED */
}

void glWaitSync(void *sync, unsigned flags, uint64_t timeout) {
    if (!sync_valid((uintptr_t)sync) || flags != 0 ||
        timeout != 0xFFFFFFFFFFFFFFFFull /* GL_TIMEOUT_IGNORED */)
        gexgl_set_error(0x0501u); /* GL_INVALID_VALUE */
}

void glGetSynciv(void *sync, unsigned pname, int buf_size, int *length, int *values) {
    if (!sync_valid((uintptr_t)sync)) {
        gexgl_set_error(0x0501u); /* GL_INVALID_VALUE */
        return;
    }
    if (buf_size < 0) {
        gexgl_set_error(0x0501u);
        return;
    }
    int value;
    switch (pname) {
    case 0x9112: value = 0x9116; break; /* GL_OBJECT_TYPE: GL_SYNC_FENCE */
    case 0x9113: value = 0x9117; break; /* GL_SYNC_CONDITION */
    case 0x9114: value = 0x9119; break; /* GL_SYNC_STATUS: GL_SIGNALED */
    case 0x9115: value = 0; break;      /* GL_SYNC_FLAGS */
    default:
        gexgl_set_error(0x0500u); /* GL_INVALID_ENUM */
        return;
    }
    if (buf_size > 0 && values) {
        values[0] = value;
        if (length) *length = 1;
    } else if (length) {
        *length = 0;
    }
}

/* Registry-derived, pointer-free OpenGL 4.4 entry points.  This include is
   regenerated by tools/gen_gl44_transport.py and uses the same batch helpers
   as the hand-written pointer/object-aware commands above. */
#include "gl44-generated.inc"

/* ------------------------------------------- the registry's other commands */

/* Every other registry command with a wire plan (tools/gen_gl_compat.py,
   guest/gl/gexgl-plan.h) is one call to this, its arguments as 64-bit words
   (floats and doubles as their bits, pointers as addresses): the plan says
   how each goes on the wire and how many elements an array holds. A command
   with a return value or outputs waits for the reply. */
static uint64_t gexgen_dbits(double d) {
    uint64_t v;
    memcpy(&v, &d, 8);
    return v;
}

static size_t gexgen_elements(const struct gexgen_tok *t, const uint64_t *v) {
    const void *ptr = (const void *)(uintptr_t)0;
    switch (t->spec) {
    case 'n': return (size_t)t->arg[0];
    case 'p': return gexgen_count((int32_t)v[t->arg[0]], t->arg[1]);
    case 'e': return gexgl_pname_count((unsigned)v[t->arg[0]]);
    case 'z': return gexgen_count((long long)v[t->arg[0]], 1);
    case 'L': return gexgen_count((int32_t)v[t->arg[0]], (long long)gexgen_type_bytes((unsigned)v[t->arg[1]]));
    case 'T': return 128;
    case 'U': return 16; /* a uniform: the host says how many, at most a 4x4 matrix */
    case 'B': return gexgen_bitmap_bytes_store((int32_t)v[t->arg[0]], (int32_t)v[t->arg[1]], &unpack_store);
    case 'X': {
        struct gexgl_pixel_layout layout;
        const int w = (int32_t)v[t->arg[2]];
        const int h = t->arg[3] >= 0 ? (int32_t)v[t->arg[3]] : 1;
        const int d = t->arg[4] >= 0 ? (int32_t)v[t->arg[4]] : -1;
        return gexgl_compute_pixel_layout((uint32_t)v[t->arg[0]], (uint32_t)v[t->arg[1]], w, h, d,
                                          &unpack_store, &layout) ? layout.end : 0;
    }
    default: (void)ptr; return 0;
    }
}

static uint64_t gexgen_call(uint32_t opcode, const char *plan, const uint64_t *v) {
    /* GEXGL_TRACE_GENERATED=1: each generated opcode the first time it is
       called (names in crates/gex-gl/src/compat_plans.rs). */
    static int trace = -1;
    if (trace < 0) trace = getenv("GEXGL_TRACE_GENERATED") != NULL;
    if (trace) {
        static unsigned char seen[8192];
        unsigned i = opcode - 8192u;
        if (i < 8192u * 8u && !(seen[i / 8] & (1u << (i % 8)))) {
            seen[i / 8] |= (unsigned char)(1u << (i % 8));
            fprintf(stderr, "gexgl: generated %u (%s)\n", (unsigned)opcode, plan);
        }
    }
    struct gexgen_tok toks[GEXGEN_MAX_PARAMS + 1];
    toks[0].kind = 'v';
    size_t bytes[GEXGEN_MAX_PARAMS];
    int mode[GEXGEN_MAX_PARAMS]; /* inputs: 0 null, 1 data, 2 unpack offset */
    int n = 0, outputs = 0;
    const char *p = gexgen_parse(plan, &toks[0]);
    const char ret = toks[0].kind;
    while (p && n < GEXGEN_MAX_PARAMS && (p = gexgen_parse(p, &toks[n + 1]))) n++;
    size_t words = 0;
    for (int i = 0; i < n; i++) {
        const struct gexgen_tok *t = &toks[i + 1];
        const void *ptr = (const void *)(uintptr_t)v[i];
        switch (t->kind) {
        case 'w': case 'f': words += 1; break;
        case 'q': words += 2; break;
        case 'o': outputs++; break;
        default:
            if (t->kind == 'j' && bound_buffer(0x88ECu)) { /* GL_PIXEL_UNPACK_BUFFER */
                mode[i] = 2; words += 3; break;
            }
            if (!ptr) { mode[i] = 0; words += 1; break; }
            if (t->spec == 's') bytes[i] = strlen((const char *)ptr);
            else if (t->spec == 'S') {
                const int len = (int32_t)v[t->arg[0]];
                bytes[i] = len < 0 ? strlen((const char *)ptr) : (size_t)len;
            } else bytes[i] = gexgen_elements(t, v) * t->size;
            if (bytes[i] > GEXGEN_MAX_BYTES) bytes[i] = 0;
            mode[i] = 1;
            words += 2 + (bytes[i] + 3) / 4;
        }
    }
    uint32_t *a = begin(opcode, words, 0);
    if (!a) return 0;
    for (int i = 0; i < n; i++) {
        const struct gexgen_tok *t = &toks[i + 1];
        switch (t->kind) {
        case 'w': case 'f': *a++ = (uint32_t)v[i]; break;
        case 'q': *a++ = (uint32_t)v[i]; *a++ = (uint32_t)(v[i] >> 32); break;
        case 'o': break;
        default:
            *a++ = (uint32_t)mode[i];
            if (mode[i] == 2) { *a++ = (uint32_t)v[i]; *a++ = (uint32_t)(v[i] >> 32); }
            else if (mode[i] == 1) {
                put_blob(a, (const void *)(uintptr_t)v[i], bytes[i]);
                a += 1 + (bytes[i] + 3) / 4;
            }
        }
    }
    if (ret == 'v' && !outputs) return 0;
    flush();
    uint64_t r = 0;
    if (ret == 'w' || ret == 'f') r = reply_u32();
    else if (ret == 'q') { r = reply_u32(); r |= (uint64_t)reply_u32() << 32; }
    for (int i = 0; i < n; i++) {
        const struct gexgen_tok *t = &toks[i + 1];
        if (t->kind != 'o') continue;
        uint32_t len = 0;
        const uint8_t *src = reply_blob(&len);
        const size_t want = gexgen_elements(t, v) * t->size;
        void *dst = (void *)(uintptr_t)v[i];
        if (dst && src && (len == want || (t->spec == 'U' && len <= want))) memcpy(dst, src, len);
    }
    return r;
}

#include "gl-compat-generated.inc"

/* --------------------------------------------------------------------- EGL */

/* EGL here is bookkeeping and nothing else. There is one display, one config
 * and one context, because the host has one and a guest that wants several
 * would need the host to as well. What matters is that an application's
 * initialisation sequence succeeds and it gets to its first draw. */

#define EGL_TRUE 1
#define EGL_FALSE 0

static int egl_ready;
static void *egl_current_display;
static void *egl_current_context;
static void *egl_current_draw;
static void *egl_current_read;
/* The window surfaces this process has, and which one is current.
 *
 * `eglCreateWindowSurface` used to return the same made-up handle to every
 * caller, which is fine for a client with one window and wrong for anything
 * else: the host kept one drawable, so a second window's frames went into the
 * first window's, and whichever surface attached last owned them both. A
 * handle per surface is what lets the host keep a drawable per window and the
 * compositor put each frame where it belongs.
 *
 * Eight, because a client with more windows than that drawing GL into all of
 * them is not a thing that happens; the ninth gets the handle of the first
 * free slot when one closes. */
#define GEXGL_MAX_SURFACES 8
struct gexgl_egl_surface {
    unsigned handle;     /* zero for a free slot */
    int kind;            /* GEXGL_SURFACE_X, GEXGL_SURFACE_WAYLAND, or -1 */
    unsigned id;         /* XID, or wl_surface object id */
    void *native;        /* what the application passed: an XID, or a
                            wl_egl_window. What lets a second request for the
                            same window find the surface that already exists. */
    int width, height;
    /* Wayland only, and per surface for the same reason as everything else:
       the proxy to commit at every swap and the window structure a resize is
       written into. */
    void *wl_proxy;
    const struct gexgl_wl_egl_window *wl_window;
    /* The frame callback this surface has outstanding, if any. */
    void *wl_callback;
    /* X only: swaps since the window's size was last asked (see
       `gexgl_x11_follow_size`). */
    unsigned x_swaps;
};
static struct gexgl_egl_surface egl_surfaces[GEXGL_MAX_SURFACES];
static unsigned egl_next_surface_handle = 1;
static struct gexgl_egl_surface *egl_surface;   /* the current drawable */

/* The current surface's size, which is what eglQuerySurface answers and what
 * the pbuffer path sets. Kept beside the table rather than inside it because
 * a query can arrive before anything has been created. */
static int egl_surface_width = 256;
static int egl_surface_height = 256;

static int gexgl_x11_window_size(void *win, int *width, int *height);

/* The surface for a window this process already has one for.
 *
 * GLX has no surface object: the drawable *is* the window, and an
 * application makes it current as often as it likes -- some engines do it
 * every frame. Making a new one each time burned a slot per call and left
 * the host holding drawables for windows nothing was drawing into. */
static struct gexgl_egl_surface *egl_surface_for_native(void *native) {
    if (!native) return NULL;
    for (unsigned i = 0; i < GEXGL_MAX_SURFACES; i++)
        if (egl_surfaces[i].handle && egl_surfaces[i].native == native)
            return &egl_surfaces[i];
    return NULL;
}

/* Let go of the surfaces whose windows have gone.
 *
 * An X window can be destroyed without this library hearing about it -- SDL
 * creates a window and a context purely to read the GL strings, then throws
 * both away before making the real ones -- and a real GL driver notices when
 * the drawable stops existing. Asking the server for the geometry is that
 * test, and it is only worth doing when a new surface is wanted. */
static void egl_surface_reap(void) {
    for (unsigned i = 0; i < GEXGL_MAX_SURFACES; i++) {
        struct gexgl_egl_surface *s = &egl_surfaces[i];
        if (!s->handle || s->kind != GEXGL_SURFACE_X) continue;
        int w = 0, h = 0;
        if (gexgl_x11_window_size(s->native, &w, &h)) continue;
        CMD1(GEXGL_SURFACE_DETACH, s->handle);
        if (egl_surface == s) egl_surface = NULL;
        *s = (struct gexgl_egl_surface){ .kind = -1 };
    }
}

static struct gexgl_egl_surface *egl_surface_new(void) {
    for (unsigned i = 0; i < GEXGL_MAX_SURFACES; i++) {
        if (egl_surfaces[i].handle) continue;
        struct gexgl_egl_surface *s = &egl_surfaces[i];
        *s = (struct gexgl_egl_surface){ .handle = egl_next_surface_handle++,
                                         .kind = -1 };
        return s;
    }
    /* Nothing free -- but some of these windows may be gone. */
    egl_surface_reap();
    for (unsigned i = 0; i < GEXGL_MAX_SURFACES; i++) {
        if (egl_surfaces[i].handle) continue;
        struct gexgl_egl_surface *s = &egl_surfaces[i];
        *s = (struct gexgl_egl_surface){ .handle = egl_next_surface_handle++,
                                         .kind = -1 };
        return s;
    }
    fprintf(stderr, "gex-gl: more than %d EGL surfaces at once;"
                    " the newest shares the current one\n", GEXGL_MAX_SURFACES);
    return NULL;
}

static struct gexgl_egl_surface *egl_surface_of(void *handle) {
    const unsigned h = (unsigned)(uintptr_t)handle;
    for (unsigned i = 0; i < GEXGL_MAX_SURFACES; i++)
        if (egl_surfaces[i].handle && egl_surfaces[i].handle == h)
            return &egl_surfaces[i];
    return NULL;
}

/* Make this surface the one the host draws into, and remember its size for
 * the queries that ask. Sent even when it has not changed on the host's side
 * only when it has: a SURFACE_CURRENT per frame would be harmless but is
 * noise in a trace. */
static void egl_surface_use(struct gexgl_egl_surface *s) {
    if (!s || s == egl_surface) return;
    egl_surface = s;
    egl_surface_width = s->width > 0 ? s->width : egl_surface_width;
    egl_surface_height = s->height > 0 ? s->height : egl_surface_height;
    CMD1(GEXGL_SURFACE_CURRENT, s->handle);
}
static int egl_context_major = 3;
static int egl_context_minor;
/* Which desktop GL profile to ask the host for: 1 is compatibility, 0 leaves
 * the choice to the host, which prefers core.
 *
 * EGL's default is core, and that is what every application here wanted until
 * GLX arrived. A GLX context created the legacy way -- glXCreateContext from
 * a visual, or glXCreateNewContext with no profile attribute -- is a
 * compatibility context by definition, and the difference is not academic:
 * a core profile has no default vertex array object, so GL2-era code that
 * never creates one has every draw dropped by st before the driver sees it.
 * glmark2 swapped 1910 frames and issued exactly zero draws that way. */
static int egl_context_profile;

/* What the application handed eglGetDisplay, and which platform it named.
 *
 * Kept only so the X11 queries below can use the caller's own connection.
 * Nothing else in this library cares which display it was given: there is one
 * host context and one surface. */
static void *egl_native_display;
static unsigned egl_native_platform;

void *eglGetDisplay(void *native) {
    egl_native_display = native;
    egl_native_platform = 0;
    return (void *)1;
}

void *eglGetPlatformDisplay(unsigned platform, void *native, const intptr_t *attrs) {
    (void)attrs;
    egl_native_display = native;
    egl_native_platform = platform;
    return (void *)1;
}

/* Is the pointer we were given an Xlib Display?
 *
 * It is a `Display *` for an X11 client and a `struct wl_display *` for a
 * Wayland one, and nothing in the value says which -- dereferencing the wrong
 * one crashes the application. So:
 *
 *   - an explicit platform is believed: only EGL_PLATFORM_X11_KHR qualifies;
 *   - a plain eglGetDisplay() qualifies only when Xlib is mapped in this
 *     process and libwayland-client is not, which is exactly the case where
 *     the pointer cannot be a wl_display.
 *
 * RTLD_NOLOAD throughout: asking EGL a question must not give a Wayland-only
 * client an X dependency, and a library's absence is itself the answer. */
static void *gexgl_x11_symbol_source(void) {
    return dlopen("libX11.so.6", RTLD_LAZY | RTLD_NOLOAD);
}

static void *gexgl_x11_client_display(void) {
    if (!egl_native_display) return NULL;
    if (egl_native_platform) {
        if (egl_native_platform != 0x31D5u) return NULL; /* EGL_PLATFORM_X11 */
    } else {
        void *wayland = dlopen("libwayland-client.so.0",
                               RTLD_LAZY | RTLD_NOLOAD);
        if (wayland) return NULL;
    }
    return gexgl_x11_symbol_source() ? egl_native_display : NULL;
}

/* The X visual an EGL config corresponds to.
 *
 * EGL on X11 has to answer EGL_NATIVE_VISUAL_ID with a visual the X server
 * actually has, because that is what the application passes to
 * XCreateWindow. This library has no X connection of its own and reported 0,
 * and mesa-demos' eglut takes that straight to XGetVisualInfo, finds nothing
 * and exits: "EGLUT: failed to get an visual of id 0x0". Every EGL-on-X11
 * program fails the same way.
 *
 * The server's default visual is the right answer here: there is one screen
 * under Xwayland, and every config this library offers is the same 8/8/8/8.
 *
 * RTLD_NOLOAD is doing two jobs. It finds Xlib without pulling it into a
 * process that does not already have it -- a Wayland-only client must not
 * gain an X dependency by asking EGL a question -- and its failing is how we
 * know not to answer: no Xlib mapped means this is not an X11 display. The
 * connection is our own rather than the caller's, because what
 * eglGetDisplay() was handed is a Display* only for X11 clients and a
 * wl_display* for Wayland ones, and nothing in the pointer says which. */
static int egl_native_visual_id = -1;

static int gexgl_x11_default_visual(void) {
    if (egl_native_visual_id >= 0) return egl_native_visual_id;
    egl_native_visual_id = 0;
    void *xlib = gexgl_x11_symbol_source();
    if (!xlib) return 0;
    void *(*open_display)(const char *) = dlsym(xlib, "XOpenDisplay");
    int (*close_display)(void *) = dlsym(xlib, "XCloseDisplay");
    int (*default_screen)(void *) = dlsym(xlib, "XDefaultScreen");
    void *(*default_visual)(void *, int) = dlsym(xlib, "XDefaultVisual");
    unsigned long (*visual_id)(void *) = dlsym(xlib, "XVisualIDFromVisual");
    if (!default_screen || !default_visual || !visual_id) return 0;
    /* The caller's connection when it is safe to use, otherwise one of our
       own: the default visual is server state, so any connection answers. */
    void *display = gexgl_x11_client_display();
    void *opened = NULL;
    if (!display && open_display) display = opened = open_display(NULL);
    if (!display) return 0;
    void *visual = default_visual(display, default_screen(display));
    if (visual) egl_native_visual_id = (int)visual_id(visual);
    if (opened && close_display) close_display(opened);
    return egl_native_visual_id;
}

/* Ask the host for a context of the client API the application selected, and
   cache the strings it answers with.

   This is deliberately not done from eglInitialize: EGL's client API comes
   from eglBindAPI, which an application is entitled to call afterwards, so a
   context created at initialization time is created before anyone knows what
   it is for.  A real EGL implementation creates it in eglCreateContext, and so
   does this one now. */
static void context_create(void) {
    if (host_context_made || !ring) {
        /* Still a context as far as the application is concerned: what is
           latched is the *host's*, and the count is what tells
           eglDestroyContext whether the last one has gone. */
        if (host_context_made) egl_contexts_live++;
        return;
    }
    host_context_made = 1;
    egl_contexts_live++;
    uint32_t *p = begin(GEXGL_CONTEXT_CREATE, 4, 0);
    if (!p) return;
    p[0] = egl_api == 0x30A2u ? 1u /* desktop */ : 0u /* ES */;
    p[1] = (uint32_t)egl_context_major;
    p[2] = (uint32_t)egl_context_minor;
    /* Additive: a host that predates this reads the first three words and
       skips the rest, which is the behaviour it had before. */
    p[3] = (uint32_t)egl_context_profile;
    flush();
    /* Vendor, renderer and version come back; they are what glGetString
       would have said, and asking now saves three round trips later. */
    uint32_t n = 0;
    for (int i = 0; i < 3; i++) {
        const uint8_t *p2 = reply_blob(&n);
        if (p2) {
            /* Whatever the host's context actually is.
               This used to substitute a fixed "4.4 (Core Profile)" or
               "OpenGL ES 3.0" here and throw the host's answer away.  An
               application told 4.4 and handed a lesser context does not learn
               that from the version string; it learns it one rejected call at
               a time.  The advertised version has to follow what the backend
               can honour, not lead it. */
            char *s = malloc(n + 1);
            if (s) {
                memcpy(s, p2, n);
                s[n] = 0;
                /* GL_VENDOR 0x1F00, GL_RENDERER 0x1F01, GL_VERSION 0x1F02 */
                free(cached_strings[(0x1F00u + (unsigned)i) & 7u]);
                cached_strings[(0x1F00u + (unsigned)i) & 7u] = s;
            }
        }
    }
}

unsigned eglInitialize(void *dpy, int *major, int *minor) {
    (void)dpy;
    if (ring_open() != 0) {
        return EGL_FALSE;
    }
    /* EGL 1.5. The GL context itself is created later, from
       eglCreateContext, once eglBindAPI has said what it is for. */
    if (major) *major = 1;
    if (minor) *minor = 5;
    egl_ready = 1;
    return EGL_TRUE;
}

unsigned eglTerminate(void *dpy) { (void)dpy; return EGL_TRUE; }
unsigned eglBindAPI(unsigned api) {
    if (api != 0x30A0u && api != 0x30A2u) /* EGL_OPENGL_ES_API / EGL_OPENGL_API */
        return EGL_FALSE;
    /* Only recorded here. The context is created from eglCreateContext, and
       the version strings are whatever the host answers for it -- selecting an
       identity here would be asserting a version rather than reporting one. */
    egl_api = api;
    return EGL_TRUE;
}

unsigned eglChooseConfig(void *dpy, const int *attrs, void **configs, int size, int *num) {
    (void)dpy; (void)attrs;
    if (size > 0 && configs) configs[0] = (void *)1;
    /* EGL's count-only form passes configs=NULL,size=0 and still requires the
       total matching count. CTS and ordinary EGL config enumeration both use
       this before allocating their config array. */
    if (num) *num = 1;
    return EGL_TRUE;
}

unsigned eglGetConfigs(void *dpy, void **configs, int size, int *num) {
    return eglChooseConfig(dpy, NULL, configs, size, num);
}

unsigned eglGetConfigAttrib(void *dpy, void *config, int attr, int *value) {
    (void)dpy; (void)config;
    if (!value) return EGL_FALSE;
    switch (attr) {
    case 0x3024: /* EGL_RED_SIZE */ *value = 8; break;
    case 0x3023: /* EGL_GREEN_SIZE */ *value = 8; break;
    case 0x3022: /* EGL_BLUE_SIZE */ *value = 8; break;
    case 0x3021: /* EGL_ALPHA_SIZE */ *value = 8; break;
    case 0x3025: /* EGL_DEPTH_SIZE */ *value = 24; break;
    case 0x3026: /* EGL_STENCIL_SIZE */ *value = 8; break;
    case 0x3031: /* EGL_SAMPLES */ *value = 0; break;
    /* EGL_WINDOW_BIT as well as EGL_PBUFFER_BIT: eglCreateWindowSurface is
     * implemented -- it is what SDL, and so SuperTuxKart, goes through -- and
     * a config that does not claim it is one an application filtering on
     * EGL_SURFACE_TYPE will not choose. */
    case 0x3033: /* EGL_SURFACE_TYPE */ *value = 0x0005; break;
    case 0x302E: /* EGL_NATIVE_VISUAL_ID */
        *value = gexgl_x11_default_visual();
        break;
    case 0x302F: /* EGL_NATIVE_VISUAL_TYPE */
        *value = 0x3038; /* EGL_NONE, when there is no X visual to name */
        break;
    case 0x3040: /* EGL_RENDERABLE_TYPE */ *value = 0x0048; break; /* OpenGL | ES3 */
    case 0x3028: /* EGL_CONFIG_ID */ *value = 1; break;
    default: *value = 0; break;
    }
    return EGL_TRUE;
}

/* How big the X window an application just handed us is.
 *
 * EGL has to answer eglQuerySurface(EGL_WIDTH/HEIGHT) with the window's real
 * size; mesa-demos' es2tri asserts on it directly --
 * `es2tri.c:344: make_x_window: Assertion 'val == width' failed` -- and any
 * program that sizes its viewport from the surface renders at the wrong
 * scale without it. This library reported a fixed 256 because
 * eglCreateWindowSurface ignored the window.
 *
 * The XID is valid on any connection to the same server, so this uses its own
 * rather than the caller's, for the reason described above
 * gexgl_x11_default_visual: what eglCreateWindowSurface receives is a Window
 * for an X11 client and a `struct wl_egl_window *` for a Wayland one, and the
 * value alone does not say which. Passing a pointer to XGetGeometry as a
 * Drawable raises BadDrawable, and Xlib's default error handler *exits the
 * process*, so an error handler is installed across the call and the failure
 * simply reads as "not an X window". */
static int gexgl_x11_error(void *display, void *event) {
    (void)display; (void)event;
    return 0;
}

static int gexgl_x11_window_size(void *win, int *width, int *height) {
    /* The caller's connection, and only the caller's: XCreateWindow is
       asynchronous, and an application creates its window and asks for a
       surface without flushing in between. A separate connection therefore
       gets BadWindow for a window that certainly exists -- measured, not
       supposed. Requests on one connection are ordered, so the client's own
       sees it. */
    void *display = gexgl_x11_client_display();
    void *xlib = gexgl_x11_symbol_source();
    if (!display || !xlib) return 0;
    int (*get_geometry)(void *, unsigned long, unsigned long *, int *, int *,
                        unsigned *, unsigned *, unsigned *, unsigned *) =
        dlsym(xlib, "XGetGeometry");
    void *(*set_error_handler)(void *) = dlsym(xlib, "XSetErrorHandler");
    int (*sync_display)(void *, int) = dlsym(xlib, "XSync");
    if (!get_geometry || !set_error_handler || !sync_display) return 0;
    /* Xlib's default error handler exits the process, and this request can
       legitimately fail -- a window id we were not actually given, say. */
    void *previous = set_error_handler((void *)gexgl_x11_error);
    unsigned long root = 0;
    int x = 0, y = 0;
    unsigned w = 0, h = 0, border = 0, depth = 0;
    const int ok = get_geometry(display, (unsigned long)(uintptr_t)win, &root,
                                &x, &y, &w, &h, &border, &depth);
    sync_display(display, 0);
    set_error_handler(previous);
    if (!ok || w == 0 || h == 0) return 0;
    *width = (int)w;
    *height = (int)h;
    return 1;
}

/* libwayland-egl's window, so the `wl_surface` inside one can be found.
 *
 * The structure is libwayland-egl's ABI rather than ours, and its first field
 * says which layout it is -- either the ABI version (3, which is this one) or
 * the size of the structure, depending on the libwayland. Both name the
 * layout below, and checking for one of the two is what stops this reading a
 * pointer that is not a wl_egl_window at all. */
#define GEXGL_WL_EGL_WINDOW_VERSION 3
struct gexgl_wl_egl_window {
    const intptr_t version;
    int width, height, dx, dy;
    int attached_width, attached_height;
    void *driver_private;
    void (*resize_callback)(struct gexgl_wl_egl_window *, void *);
    void (*destroy_window_callback)(struct gexgl_wl_egl_window *);
    void *surface;
};

/* The object id of the `wl_surface` a Wayland client's window draws to, or
 * zero if this is not one.
 *
 * Only asked once the X path has said no, and only when this process has
 * libwayland-client mapped: what arrives here is an XID for an X client and a
 * pointer for a Wayland one, and dereferencing the first would be reading a
 * small integer as an address. */
/* Which `wl_surface` a window is, and the two things a swap needs from it --
 * the proxy to commit and the structure a resize is written into -- are kept
 * per surface in `egl_surfaces`, not once per process: a client with two
 * windows commits two surfaces, and committing the wrong one is a frame
 * callback that never comes for the window that is waiting.
 *
 * Fills `into` when there is one, and returns the object id either way. */
static unsigned gexgl_wl_surface_id_into(void *win,
                                         struct gexgl_egl_surface *into) {
    if (!win || (uintptr_t)win < 4096 || ((uintptr_t)win & (sizeof(void *) - 1)))
        return 0;
    void *wayland = dlopen("libwayland-client.so.0", RTLD_LAZY | RTLD_NOLOAD);
    if (!wayland) {
        fprintf(stderr, "gex-gl: no libwayland-client in this process\n");
        return 0;
    }
    unsigned (*proxy_id)(void *) = dlsym(wayland, "wl_proxy_get_id");
    if (!proxy_id) {
        fprintf(stderr, "gex-gl: libwayland-client has no wl_proxy_get_id\n");
        return 0;
    }
    const struct gexgl_wl_egl_window *w = win;
    if (w->version != GEXGL_WL_EGL_WINDOW_VERSION &&
        w->version != (intptr_t)sizeof *w) {
        fprintf(stderr, "gex-gl: wl_egl_window version %ld, expected %d or %zu\n",
                (long)w->version, GEXGL_WL_EGL_WINDOW_VERSION, sizeof *w);
        return 0;
    }
    if (!w->surface) return 0;
    const unsigned id = proxy_id(w->surface);
    if (into) {
        into->kind = GEXGL_SURFACE_WAYLAND;
        into->id = id;
        into->wl_proxy = w->surface;
        into->wl_window = w;
    }
    return id;
}


/* Commit the surface, which is what a real EGL does inside eglSwapBuffers.
 *
 * Nothing is attached -- the frame is on the host -- but the commit still
 * matters: it is what carries the `wl_surface.frame` callback the client
 * asked for, and a client that paces itself on that callback draws exactly
 * one frame without it and then waits for ever. es2gears did.
 *
 * `wl_surface.commit` is opcode 6, and `wl_proxy_marshal` is how a request
 * with no arguments is sent; the generated inline in the client header does
 * exactly this. */
/* Keep the host's drawable and the compositor's idea of the window the size
 * the client is actually drawing at.
 *
 * `wl_egl_window_resize` writes the new size into the structure and tells
 * nobody: a real EGL notices at the next swap, and so does this. Without it
 * the frame is one size and the window another, and the compositor stretches
 * whatever it was given. */
static void gexgl_wl_follow_size(struct gexgl_egl_surface *s) {
    if (!s || !s->wl_proxy || !s->wl_window) return;
    const struct gexgl_wl_egl_window *w = s->wl_window;
    if (w->width <= 0 || w->height <= 0) return;
    if (w->width == s->width && w->height == s->height) return;
    s->width = w->width;
    s->height = w->height;
    if (s == egl_surface) {
        egl_surface_width = w->width;
        egl_surface_height = w->height;
    }
    /* The current drawable is the one SURFACE_RESIZE means, so say which
       first. Cheap when it is already current: see `egl_surface_use`. */
    egl_surface_use(s);
    CMD2(GEXGL_SURFACE_RESIZE, w->width, w->height);
    unsigned args[4] = { GEXGL_SURFACE_WAYLAND, s->id,
                         (unsigned)w->width, (unsigned)w->height };
    ioctl(gl_fd, GEXGL_SURFACE, args);
}

/* An X window's resize, followed at swap time as a real GLX driver does: SDL
 * makes Unity's window at the desktop's size and resizes it after the context
 * exists, and without this the host kept drawing at the first size. Asking
 * the server costs a round trip, so it is asked on the first swaps and then
 * every 30th. */
static void gexgl_x11_follow_size(struct gexgl_egl_surface *s) {
    if (!s || s->kind != GEXGL_SURFACE_X) return;
    unsigned n = s->x_swaps++;
    if (n > 8 && n % 30 != 0) return;
    int w = 0, h = 0;
    if (!gexgl_x11_window_size(s->native, &w, &h)) return;
    if (w <= 0 || h <= 0 || (w == s->width && h == s->height)) return;
    s->width = w;
    s->height = h;
    if (s == egl_surface) {
        egl_surface_width = w;
        egl_surface_height = h;
    }
    egl_surface_use(s);
    CMD2(GEXGL_SURFACE_RESIZE, w, h);
}

static void gexgl_wl_commit(struct gexgl_egl_surface *s) {
    if (!s || !s->wl_proxy) return;
    void *wayland = dlopen("libwayland-client.so.0", RTLD_LAZY | RTLD_NOLOAD);
    if (!wayland) return;
    void (*marshal)(void *, unsigned) = dlsym(wayland, "wl_proxy_marshal");
    if (!marshal) return;

    /* A frame callback, asked for before the commit that carries it.
     *
     * Not for pacing -- the host's `displayed` count does that -- but because
     * the application is asleep in `wl_display_dispatch`, which returns when
     * an event arrives and not otherwise. A surface with no buffer generates
     * no release and no configure, so this callback is the only event the
     * compositor has to send, and without it es2gears drew one frame and
     * waited for ever. A real EGL asks for one here too.
     *
     * The previous one is destroyed rather than left: this happens every
     * frame, and a proxy per frame is a leak per frame. */
    void *(*marshal_ctor)(void *, unsigned, const void *, void *) =
        dlsym(wayland, "wl_proxy_marshal_constructor");
    const void *callback_interface = dlsym(wayland, "wl_callback_interface");
    void (*proxy_destroy)(void *) = dlsym(wayland, "wl_proxy_destroy");
    /* Per surface: one callback outstanding per window, destroyed when its
       replacement is asked for. Shared, a second window's commit destroyed
       the first window's pending callback and that window stopped drawing. */
    if (marshal_ctor && callback_interface) {
        if (s->wl_callback && proxy_destroy) proxy_destroy(s->wl_callback);
        s->wl_callback = marshal_ctor(s->wl_proxy, 3, callback_interface, NULL);
    }

    marshal(s->wl_proxy, 6);
    void *(*get_display)(void *) = dlsym(wayland, "wl_proxy_get_display");
    int (*flush)(void *) = dlsym(wayland, "wl_display_flush");
    if (get_display && flush) flush(get_display(s->wl_proxy));
}

void *eglCreateWindowSurface(void *dpy, void *config, void *win,
                             const int *attrs) {
    (void)dpy; (void)config; (void)attrs;
    /* One surface per window. EGL says a second request for the same window
       is an error; answering with the one that exists is what a GLX caller
       needs, since making a drawable current twice is ordinary there -- and
       it is friendlier than an error to an EGL caller that asked twice. */
    struct gexgl_egl_surface *existing = egl_surface_for_native(win);
    if (existing) {
        egl_surface_use(existing);
        return (void *)(uintptr_t)existing->handle;
    }
    egl_surface_reap();
    struct gexgl_egl_surface *s = egl_surface_new();
    if (!s) return (void *)(uintptr_t)(egl_surface ? egl_surface->handle : 1);
    int w = 0, h = 0;
    if (gexgl_x11_window_size(win, &w, &h)) {
        s->kind = GEXGL_SURFACE_X;
        s->id = (unsigned)(uintptr_t)win;
        s->native = win;
        s->width = w;
        s->height = h;
        /* Which window this surface's frames belong to, so a compositor can
           put them inside it rather than over everything: the host draws for
           a guest whose window lives somewhere else, and without this there
           is nowhere to put the pixels. The handle goes with it, which is
           what allows a second window at the same time -- the host keeps a
           drawable per handle. Attaching also makes it current. */
        CMD4(GEXGL_SURFACE_ATTACH, GEXGL_SURFACE_X, s->id,
             (unsigned)getpid(), s->handle);
        egl_surface = s;
        egl_surface_width = w;
        egl_surface_height = h;
        /* The host's drawable is what reaches the screen, so the size the
           application's window has is the size to draw at. */
        CMD2(GEXGL_SURFACE_RESIZE, w, h);
    } else if (gexgl_wl_surface_id_into(win, s)) {
        s->native = win;
        const struct gexgl_wl_egl_window *ww = win;
        if (ww->width > 0 && ww->height > 0) {
            s->width = ww->width;
            s->height = ww->height;
        }
        CMD4(GEXGL_SURFACE_ATTACH, GEXGL_SURFACE_WAYLAND, s->id,
             (unsigned)getpid(), s->handle);
        egl_surface = s;
        if (s->width > 0 && s->height > 0) {
            egl_surface_width = s->width;
            egl_surface_height = s->height;
            CMD2(GEXGL_SURFACE_RESIZE, s->width, s->height);
        }
        /* And tell this process's own compositor, which is a different thing
           from telling the host: nothing will ever be attached to this
           surface -- the frames are drawn on the host and never come back --
           so without this it stays unmapped and is never shown at all. */
        unsigned args[4] = { GEXGL_SURFACE_WAYLAND, s->id,
                             (unsigned)egl_surface_width,
                             (unsigned)egl_surface_height };
        const int marked = ioctl(gl_fd, GEXGL_SURFACE, args);
        fprintf(stderr, "gex-gl: wl_surface %u is drawn on the host,"
                        " %dx%d (%s)\n",
                s->id, egl_surface_width, egl_surface_height,
                marked == 0 ? "ok" : strerror(errno));
    } else {
        fprintf(stderr, "gex-gl: eglCreateWindowSurface: %p is neither an X"
                        " window nor a wl_egl_window\n", win);
        /* Still a surface, and still a drawable of its own: a client that
           goes on to draw into it gets its frames rendered, they simply have
           no window to be composited into. */
        egl_surface_use(s);
    }
    return (void *)(uintptr_t)s->handle;
}

void *eglCreatePbufferSurface(void *dpy, void *config, const int *attrs) {
    (void)dpy; (void)config;
    /* A drawable of its own, with no window attached to it: offscreen is
       exactly what a pbuffer is, and a client that has one as well as a
       window must not have them share. */
    struct gexgl_egl_surface *s = egl_surface_new();
    if (!s) return (void *)(uintptr_t)(egl_surface ? egl_surface->handle : 1);
    int w = 0, h = 0;
    if (attrs) {
        for (const int *a = attrs; *a != 0x3038; a += 2) {
            if (a[0] == 0x3057) w = a[1];      /* EGL_WIDTH */
            else if (a[0] == 0x3056) h = a[1]; /* EGL_HEIGHT */
        }
    }
    s->width = w;
    s->height = h;
    egl_surface_use(s);
    if (w > 0 && h > 0) {
        egl_surface_width = w;
        egl_surface_height = h;
        CMD2(GEXGL_SURFACE_RESIZE, w, h);
    }
    return (void *)(uintptr_t)s->handle;
}

unsigned eglDestroySurface(void *dpy, void *surface) {
    (void)dpy;
    struct gexgl_egl_surface *s = egl_surface_of(surface);
    if (!s) return EGL_TRUE;
    /* The host holds a drawable, a colour buffer and a depth buffer for this
       surface, and -- for a window -- a frame the compositor may still be
       drawing. Saying it is gone is what frees them; a client that exits
       without saying so is cleaned up by the page instead, which notices the
       process has gone. */
    CMD1(GEXGL_SURFACE_DETACH, s->handle);
    flush();
    if (egl_surface == s) egl_surface = NULL;
    *s = (struct gexgl_egl_surface){ .kind = -1 };
    return EGL_TRUE;
}

void *eglCreateContext(void *dpy, void *config, void *share, const int *attrs) {
    (void)dpy; (void)config; (void)share;
    /* EGL's defaults: version 1.0, which for desktop GL means a context of
       whatever version the implementation offers that is backward compatible
       with 1.0 -- the compatibility profile, since a core one is defined only
       from 3.2 and the profile mask is ignored below it. (4.4 here made an
       application that asks for no version, SDL's default, get a 4.4 core
       context with no fixed function on a host that honours the profile.) */
    egl_context_major = egl_api == 0x30A2u ? 1 : 3;
    egl_context_minor = 0;
    egl_context_profile = 0;
    if (attrs) {
        for (const int *a = attrs; *a != 0x3038; a += 2) {
            if (a[0] == 0x3098) egl_context_major = a[1];
            else if (a[0] == 0x30FB) egl_context_minor = a[1];
            /* EGL_CONTEXT_OPENGL_PROFILE_MASK, which EGL 1.5 defines and this
               used to ignore: bit 0 is core, bit 1 compatibility. Core stays
               the default, as EGL specifies, so this only ever widens what an
               application can ask for. */
            else if (a[0] == 0x30FD) egl_context_profile = (a[1] & 0x2) ? 1 : 0;
        }
    }
    context_create();
    return (void *)3;
}

/* Really destroy it, so the next context is created rather than latched.
 *
 * The host makes its GL context when this library first says what the
 * application asked for, and that used to be the only time it ever made one:
 * a program that created a context, threw it away and made another with
 * different attributes -- which is how SDL reads the GL strings before
 * choosing a profile -- silently kept the first one's profile and version.
 *
 * Only the last one: an application that keeps two contexts and destroys one
 * is not asking for the other's to go. */
unsigned eglDestroyContext(void *dpy, void *ctx) {
    (void)dpy; (void)ctx;
    if (!host_context_made || egl_contexts_live > 1) {
        if (egl_contexts_live) egl_contexts_live--;
        return EGL_TRUE;
    }
    egl_contexts_live = 0;
    host_context_made = 0;
    egl_current_context = NULL;
    CMD0(GEXGL_CONTEXT_DESTROY);
    flush();
    return EGL_TRUE;
}

unsigned eglMakeCurrent(void *dpy, void *draw, void *read, void *ctx) {
    if (!egl_ready) return EGL_FALSE;
    /* A safety net for callers that reach a current context without going
       through this library's eglCreateContext. The latch inside makes it a
       no-op once the host context exists. */
    if (ctx) context_create();
    egl_current_display = ctx ? dpy : NULL;
    egl_current_context = ctx;
    egl_current_draw = ctx ? draw : NULL;
    egl_current_read = ctx ? read : NULL;
    /* Which drawable the host should render into from here on. This is the
       call that means it, and the reason a client with two windows can draw
       both: without it the host drew everything into whichever surface was
       created last. */
    if (ctx && draw) egl_surface_use(egl_surface_of(draw));
    return EGL_TRUE;
}

/* EGL's default is 1 -- an application that never calls eglSwapInterval is
   asking to be paced, and most never call it. */
static int egl_swap_interval = 1;
static uint32_t egl_swaps_issued;

unsigned eglSwapBuffers(void *dpy, void *surface) {
    (void)dpy;
    /* The surface named here, not the one that happens to be current: a
       client that draws two windows in one frame swaps each in turn. */
    struct gexgl_egl_surface *s = egl_surface_of(surface);
    if (s) egl_surface_use(s);
    else s = egl_surface;
    /* Preserve the swap boundary in the wire stream. WebGL displays its
       default framebuffer automatically, while explicit APIs such as WebGPU
       must acquire and submit a surface image here. */
    // What a real EGL does here as well as presenting: notice a resize, and
    // commit -- the commit is what carries the frame callback this client is
    // waiting for.
    gexgl_wl_follow_size(s);
    gexgl_x11_follow_size(s);
    gexgl_wl_commit(s);
    CMD0(GEXGL_PRESENT);
    flush();
    /* How many frames the page has actually shown. Zero from a decoder that
       predates this reply, which is why nothing below acts on zero. */
    const uint32_t shown = reply_u32();
    ++egl_swaps_issued;

    /* Pacing, which is what swap interval means here.
     *
     * The backend copies a rendered frame to the canvas only when the page's
     * animation loop asks it to, so an application that swaps as fast as it
     * can does not go faster -- it goes slower. es2gears issued 400 swaps a
     * second to put 15 frames on screen, and the round trips alone -- 0.3 ms
     * each, 45% of its wall time -- starved the very loop it was waiting for.
     * Pacing it took the same program from 17 FPS to 59.9.
     *
     * This is a feedback loop, not a frame rate. 16.7 ms is its step size:
     * being one frame ahead sleeps about that long, and whether that was too
     * much or too little shows up in the next reply. A page displaying faster
     * keeps `ahead` small and barely sleeps; a page displaying slower lets it
     * grow and sleeps more. Nothing here assumes a refresh rate.
     *
     * `shown == 0` means either a decoder that does not report, or a page
     * with no display loop at all. Neither is a reason to slow the guest
     * down, and the second used to be the gears demo. */
    if (egl_swap_interval > 0 && shown > 0 && egl_swaps_issued > shown) {
        const uint32_t ahead = egl_swaps_issued - shown;
        /* One frame in flight is normal and must not be paced against: the
           page displays a frame after the swap that produced it, so `ahead`
           sits at 1 for a well-behaved application and briefly at 2 whenever
           the display loop misses a beat. Pacing at 1 cost SuperTuxKart a
           fifth of its frame rate -- 60 down to 48.8 -- for nothing. */
        const uint32_t slack = (uint32_t)egl_swap_interval + 1u;
        if (ahead > slack) {
            unsigned long micros = (unsigned long)(ahead - slack) * 16667UL;
            /* Capped so a page that stops displaying entirely -- a hidden
               tab -- stalls the application rather than wedging it. */
            if (micros > 50000UL) micros = 50000UL;
            usleep(micros);
        }
    }
    return EGL_TRUE;
}

unsigned eglSwapInterval(void *dpy, int interval) {
    (void)dpy;
    egl_swap_interval = interval < 0 ? 0 : interval;
    return EGL_TRUE;
}
int eglGetError(void) { return 0x3000; /* EGL_SUCCESS */ }

void *eglGetCurrentDisplay(void) { return egl_current_display; }
void *eglGetCurrentContext(void) { return egl_current_context; }
void *eglGetCurrentSurface(int which) {
    return which == 0x305Au /* EGL_READ */ ? egl_current_read : egl_current_draw;
}
unsigned eglQueryAPI(void) { return egl_api; }

unsigned eglQueryContext(void *dpy, void *ctx, int attr, int *value) {
    (void)dpy; (void)ctx;
    if (!value) return EGL_FALSE;
    switch (attr) {
    case 0x3097: *value = (int)egl_api; break;       /* EGL_CONTEXT_CLIENT_TYPE */
    case 0x3098: *value = egl_context_major; break; /* EGL_CONTEXT_CLIENT_VERSION */
    case 0x30FB: *value = egl_context_minor; break; /* EGL_CONTEXT_MINOR_VERSION */
    case 0x3028: *value = 1; break;                 /* EGL_CONFIG_ID */
    default: *value = 0; break;
    }
    return EGL_TRUE;
}

unsigned eglQuerySurface(void *dpy, void *surface, int attr, int *value) {
    (void)dpy; (void)surface;
    if (!value) return EGL_FALSE;
    switch (attr) {
    case 0x3057: *value = egl_surface_width; break;  /* EGL_WIDTH */
    case 0x3056: *value = egl_surface_height; break; /* EGL_HEIGHT */
    case 0x3028: *value = 1; break;                  /* EGL_CONFIG_ID */
    default: *value = 0; break;
    }
    return EGL_TRUE;
}

unsigned eglReleaseThread(void) {
    egl_current_display = egl_current_context = NULL;
    egl_current_draw = egl_current_read = NULL;
    return EGL_TRUE;
}
unsigned eglWaitClient(void) { flush(); return EGL_TRUE; }
unsigned eglWaitGL(void) { flush(); return EGL_TRUE; }
unsigned eglWaitNative(int engine) { (void)engine; return EGL_TRUE; }
unsigned eglSurfaceAttrib(void *dpy, void *surface, int attr, int value) {
    (void)dpy; (void)surface; (void)attr; (void)value; return EGL_TRUE;
}
unsigned eglBindTexImage(void *dpy, void *surface, int buffer) {
    (void)dpy; (void)surface; (void)buffer; return EGL_FALSE;
}
unsigned eglReleaseTexImage(void *dpy, void *surface, int buffer) {
    (void)dpy; (void)surface; (void)buffer; return EGL_FALSE;
}
unsigned eglCopyBuffers(void *dpy, void *surface, void *pixmap) {
    (void)dpy; (void)surface; (void)pixmap; return EGL_FALSE;
}
void *eglCreatePixmapSurface(void *dpy, void *config, void *pixmap, const int *attrs) {
    (void)dpy; (void)config; (void)pixmap; (void)attrs; return NULL;
}
void *eglCreatePlatformWindowSurface(void *dpy, void *config, void *window,
                                     const intptr_t *attrs) {
    (void)attrs;
    return eglCreateWindowSurface(dpy, config, window, NULL);
}
void *eglCreatePlatformPixmapSurface(void *dpy, void *config, void *pixmap,
                                     const intptr_t *attrs) {
    (void)dpy; (void)config; (void)pixmap; (void)attrs; return NULL;
}
void *eglCreatePbufferFromClientBuffer(void *dpy, unsigned type, void *buffer,
                                       void *config, const int *attrs) {
    (void)dpy; (void)type; (void)buffer; (void)config; (void)attrs; return NULL;
}
void *eglCreateImage(void *dpy, void *ctx, unsigned target, void *buffer,
                     const intptr_t *attrs) {
    (void)dpy; (void)ctx; (void)target; (void)buffer; (void)attrs; return NULL;
}
unsigned eglDestroyImage(void *dpy, void *image) {
    (void)dpy; (void)image; return EGL_FALSE;
}
void *eglCreateSync(void *dpy, unsigned type, const intptr_t *attrs) {
    (void)dpy; (void)type; (void)attrs; return NULL;
}
unsigned eglDestroySync(void *dpy, void *sync) {
    (void)dpy; (void)sync; return EGL_FALSE;
}
int eglClientWaitSync(void *dpy, void *sync, int flags, uint64_t timeout) {
    (void)dpy; (void)sync; (void)flags; (void)timeout; return 0x30F5; /* EGL_TIMEOUT_EXPIRED */
}
unsigned eglGetSyncAttrib(void *dpy, void *sync, int attr, intptr_t *value) {
    (void)dpy; (void)sync; (void)attr; (void)value; return EGL_FALSE;
}
unsigned eglWaitSync(void *dpy, void *sync, int flags) {
    (void)dpy; (void)sync; (void)flags; return EGL_FALSE;
}

const char *eglQueryString(void *dpy, int name) {
    (void)dpy;
    switch (name) {
    /* These two were the wrong way round, and the comments said so with
       confidence: 0x3053 is EGL_VENDOR and 0x3054 is EGL_VERSION. eglinfo
       printed "EGL vendor string: 1.5" and "EGL version string: gex". */
    case 0x3053: return "gex";       /* EGL_VENDOR */
    /* EGL requires "<major>.<minor> <vendor-specific>", and applications do
       parse the number off the front. */
    case 0x3054: return "1.5 gex";   /* EGL_VERSION */
    case 0x3055: return "";          /* EGL_EXTENSIONS */
    case 0x308D: return "OpenGL OpenGL_ES"; /* EGL_CLIENT_APIS */
    default: return "";
    }
}

/* An application that looks a function up by name gets the same one it would
   have linked against, so a loader-style application works unchanged.

   Looked up in this object itself first, found by the address of this very
   function: a host's own GL can be in the process too (a glibc guest that
   dlopens the system libEGL or libOpenGL beside this one), and a global
   lookup, or a reopen by soname, would hand back its functions, which have
   no context. CTS loads libEGL with RTLD_LOCAL, and on musl RTLD_DEFAULT
   does not search that object's scope at all, which is the other reason the
   lookup goes through a handle on this object. */
void *eglGetProcAddress(const char *name) {
    if (!name) return NULL;
    static void *self;
    if (!self) {
        Dl_info info;
        if (dladdr((void *)eglGetProcAddress, &info) && info.dli_fname)
            self = dlopen(info.dli_fname, RTLD_LAZY | RTLD_LOCAL | RTLD_NOLOAD);
        if (!self) self = dlopen("libEGL.so.1", RTLD_LAZY | RTLD_LOCAL | RTLD_NOLOAD);
        if (!self) self = dlopen("libGLESv2.so.2", RTLD_LAZY | RTLD_LOCAL | RTLD_NOLOAD);
    }
    void *proc = self ? dlsym(self, name) : NULL;
    return proc ? proc : dlsym(RTLD_DEFAULT, name);
}

/* Fixed function: the compatibility-profile calls a pre-shader program makes.
 *
 * Not generated, because tools/gen_gl44_manifest.py builds its command list
 * from glcorearb.h and the core-profile header omits every one of these by
 * definition. Twenty of them is what glxgears needs and it needs nothing
 * else, so they are written out rather than the manifest machinery being
 * taught a second profile.
 *
 * Immediate mode is as chatty as it looks. It is tolerable here because a
 * program of this era builds its geometry into display lists once -- glxgears
 * does -- after which a frame is a few matrix calls and three glCallList. A
 * program that submits vertices every frame through this will be slow, and
 * should be, because that is what it is asking for. */

/* A double as two words, so glFrustum keeps the precision its projection
   depends on rather than being rounded to float on the way out. */
static void put_double(uint32_t *at, double value) {
    uint64_t bits;
    memcpy(&bits, &value, 8);
    at[0] = (uint32_t)(bits & 0xffffffffu);
    at[1] = (uint32_t)(bits >> 32);
}

static void put_float(uint32_t *at, float value) { memcpy(at, &value, 4); }

void glMatrixMode(unsigned mode) { CMD1(GEXGL_MATRIX_MODE, mode); }
void glLoadIdentity(void) { CMD0(GEXGL_LOAD_IDENTITY); }
void glPushMatrix(void) { CMD0(GEXGL_PUSH_MATRIX); }
void glPopMatrix(void) { CMD0(GEXGL_POP_MATRIX); }
void glBegin(unsigned mode) { CMD1(GEXGL_BEGIN, mode); }
void glEnd(void) { CMD0(GEXGL_END); }
void glShadeModel(unsigned mode) { CMD1(GEXGL_SHADE_MODEL, mode); }
void glEndList(void) { CMD0(GEXGL_END_LIST); }
void glCallList(unsigned list) { CMD1(GEXGL_CALL_LIST, list); }
void glDeleteLists(unsigned list, int range) {
    CMD2(GEXGL_DELETE_LISTS, list, (unsigned)range);
}
void glNewList(unsigned list, unsigned mode) {
    CMD2(GEXGL_NEW_LIST, list, mode);
}

void glTranslatef(float x, float y, float z) {
    uint32_t *p = begin(GEXGL_TRANSLATEF, 3, 0); if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y); put_float(&p[2], z);
}
/* GL's double entry point, carried as float: the matrix stack this feeds is
   float either way, and glxgears uses it for a camera offset. glFrustum below
   is the one where the extra range matters, and that one keeps it. */
void glTranslated(double x, double y, double z) {
    glTranslatef((float)x, (float)y, (float)z);
}
void glRotatef(float angle, float x, float y, float z) {
    uint32_t *p = begin(GEXGL_ROTATEF, 4, 0); if (!p) return;
    put_float(&p[0], angle); put_float(&p[1], x);
    put_float(&p[2], y); put_float(&p[3], z);
}
void glVertex3f(float x, float y, float z) {
    uint32_t *p = begin(GEXGL_VERTEX3F, 3, 0); if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y); put_float(&p[2], z);
}
void glNormal3f(float x, float y, float z) {
    uint32_t *p = begin(GEXGL_NORMAL3F, 3, 0); if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y); put_float(&p[2], z);
}
void glFrustum(double l, double r, double b, double t, double n, double f) {
    uint32_t *p = begin(GEXGL_FRUSTUM, 12, 0); if (!p) return;
    put_double(&p[0], l); put_double(&p[2], r); put_double(&p[4], b);
    put_double(&p[6], t); put_double(&p[8], n); put_double(&p[10], f);
}

/* How many floats a light or material parameter carries. The colour and
   position parameters take four, spot direction three, and the scalars one;
   sending the wrong number would read past the caller's array. */
static size_t light_parameter_floats(unsigned pname) {
    switch (pname) {
    case 0x1200: case 0x1201: case 0x1202: case 0x1203: return 4;
        /* AMBIENT, DIFFUSE, SPECULAR, POSITION */
    case 0x1204: return 3;                       /* SPOT_DIRECTION */
    case 0x1205: case 0x1206: case 0x1207:       /* SPOT_EXPONENT/CUTOFF */
    case 0x1208: case 0x1209:                    /* attenuations */
        return 1;
    default: return 4;
    }
}

static size_t material_parameter_floats(unsigned pname) {
    switch (pname) {
    case 0x1600: return 4;   /* GL_EMISSION */
    case 0x1601: return 1;   /* GL_SHININESS */
    case 0x1602: return 3;   /* GL_AMBIENT_AND_DIFFUSE is 4; COLOR_INDEXES 3 */
    case 0x1200: case 0x1201: case 0x1202: return 4;
    default: return 4;
    }
}

static void put_parameter(uint32_t opcode, unsigned face, unsigned pname,
                          const float *values, size_t count) {
    if (!values || !count) return;
    uint32_t *p = begin(opcode, 3 + count, 0);
    if (!p) return;
    p[0] = face;
    p[1] = pname;
    p[2] = (uint32_t)count;
    for (size_t i = 0; i < count; ++i) put_float(&p[3 + i], values[i]);
}

void glLightfv(unsigned light, unsigned pname, const float *params) {
    put_parameter(GEXGL_LIGHTFV, light, pname, params,
                  light_parameter_floats(pname));
}
void glMaterialfv(unsigned face, unsigned pname, const float *params) {
    put_parameter(GEXGL_MATERIALFV, face, pname, params,
                  material_parameter_floats(pname));
}

/* The host's own list names are handed straight back. Display lists are the
   only object here the guest does not allocate names for, and glGenLists is
   their only source, so guest and host cannot disagree about them. */
unsigned glGenLists(int range) {
    CMD1(GEXGL_GEN_LISTS, (unsigned)range);
    flush();
    return reply_u32();
}

/* GLX, exported from libGL.so.1. After the EGL above, which it is built on.
 */
#include "gexgl-glx.inc"

/* Registry command aliases; see tools/gen_gl_extensions.py. Last, so every
   alias target above is already declared. */
#include "gl-aliases-generated.inc"
