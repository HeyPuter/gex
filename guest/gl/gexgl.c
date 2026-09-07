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

/* The error latch, declared before anything that could raise one. Its
   definition lives with glGetError much further down, but entry points that
   latch GL's errors are scattered throughout -- including well above that
   point -- and a macro defined halfway down the file is a link error waiting
   for the next one added above it, which is exactly how it failed. */
static void gexgl_set_error(unsigned error);
static void gexgl_set_error_at(unsigned error, const char *who);
#define GEXGL_SET_ERROR(e) gexgl_set_error_at((e), __func__)

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
/* Walk the queued batch and check that every command's declared length lands
 * exactly on the next header, and that the last one ends exactly at `tail`.
 *
 * begin() advances `tail` by the same length it writes into the header, so the
 * host and the guest can never disagree about where a command *ends* -- which
 * means a decode failure on the host is not a size mismatch, it is a command
 * that wrote past its own reservation and overwrote the next header with data.
 * The host sees only the wreckage: an opcode that is really a fragment of
 * somebody's payload, and no way to name the command that scribbled it.
 *
 * Here the writer is still in scope. GEX_GL_CHECK_BATCH=1 turns it on.
 */
static void gexgl_check_batch(void) {
    static int enabled = -1;
    if (enabled < 0) enabled = getenv("GEX_GL_CHECK_BATCH") ? 1 : 0;
    if (!enabled || !ring) return;
    uint32_t at = 0, previous_op = 0, previous_at = 0;
    while (at < ring->tail) {
        if (ring->tail - at < 8) {
            fprintf(stderr, "gexgl: batch tail %u has %u stray bytes after "
                            "opcode %u at %u\n",
                    ring->tail, ring->tail - at, previous_op, previous_at);
            return;
        }
        uint32_t op, size;
        memcpy(&op, cmds + at, 4);
        memcpy(&size, cmds + at + 4, 4);
        if (size < 8 || size > ring->tail - at) {
            fprintf(stderr,
                    "gexgl: malformed header at %u (opcode %u size %u, %u left) "
                    "-- written by opcode %u at %u, which overran\n",
                    at, op, size, ring->tail - at, previous_op, previous_at);
            return;
        }
        previous_op = op;
        previous_at = at;
        at += size;
    }
    /* Ends exactly on the boundary, or the host will read past the last
       command into whatever the previous batch left behind. */
    if (at != ring->tail)
        fprintf(stderr, "gexgl: batch ends at %u but tail is %u (last opcode %u)\n",
                at, ring->tail, previous_op);
}

static void flush(void) {
    if (!ring || ring->tail == 0) {
        reply_at = 0;
        return;
    }
    gexgl_check_batch();
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
            fprintf(stderr,
                    "gexgl: submit failed, discarding %u queued bytes "
                    "(status %u) -- GL commands are being lost\n",
                    ring->tail, ring->status);
        }
        ring->tail = 0;
        ring->reply_len = 0;
    }
    reply_at = 0;
}

/* Start a command. Returns where to write its arguments, having reserved
   room for them, or NULL if the batch is full -- in which case it is flushed
   and the caller tries again. */
/* GEX_GL_TRACE_ERRORS=1 asks the host for its error after every single
   command and names the opcode that raised one.

   A spurious GL error is otherwise close to undiagnosable from this side. The
   suite checks glGetError once at the end of a case and reports whatever it
   finds against the whole case -- 39 of the 42 compute_shader cases fail that
   way while their own bodies report "Everything works as expected" -- and the
   guest's own latch, which GEX_GL_LOG_ERRORS names, is innocent, so the call
   that raised it is somewhere in a stream of thousands.

   One flush and one round trip per command, so this is a diagnosis and never
   a measurement. */
unsigned glGetError(void);

static int gexgl_trace_errors(void) {
    static int on = -1;
    if (on < 0) on = getenv("GEX_GL_TRACE_ERRORS") ? 1 : 0;
    return on;
}

static void gexgl_watch_errors(uint32_t next_opcode) {
    static int on = -1;
    static int inside;
    static uint32_t previous;
    if (on < 0) {
        const char *v = getenv("GEX_GL_TRACE_ERRORS");
        on = v ? atoi(v) : 0;
        if (v && on < 1) on = 1;
    }
    if (!on || inside) return;
    inside = 1;
    if (on > 1) fprintf(stderr, "gexgl: op %u\n", next_opcode);
    if (previous) {
        const unsigned error = glGetError();
        if (error)
            fprintf(stderr, "gexgl: host error 0x%04x after opcode %u\n",
                    error, previous);
    }
    previous = next_opcode;
    inside = 0;
}

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
    gexgl_watch_errors(opcode);
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
/* ARB_polygon_offset_clamp. Core in GL 4.6, which this driver now reports,
   so an application that checks the version and calls it has to find it --
   the extension string is generated from what this library exports, and the
   version came from the host either way. */
void glPolygonOffsetClamp(float factor, float units, float clamp) {
    CMD3(GEXGL_POLYGON_OFFSET_CLAMP, as_bits(factor), as_bits(units),
         as_bits(clamp));
}
/* No glPolygonOffsetClampEXT alias here: gl-aliases-generated.inc emits one
   for every registry alias whose base function this library defines, and it
   started emitting this one as soon as the base appeared above. */
/* KHR_blend_equation_advanced. The barrier is the whole extension's contract:
   it tells the driver that the draws after it read what the draws before it
   wrote, which is how a non-coherent implementation -- ours, via a colour
   attachment snapshot -- knows when to take one.

   Not generated, because it is an extension entry point and the manifest
   holds the GL 4.4 core set; and it cannot join that set anyway, since the
   generated opcodes are indices into the sorted core command list and a name
   inserted near the front would renumber almost the whole ABI. It takes a
   hand-written opcode beside glPolygonOffsetClamp instead.

   Without it the extension was *withheld*: gen_gl_extensions.py claims an
   extension only when this library exports every command the registry says
   it requires, and glBlendBarrier was the one missing. So the host driver
   passed 87 of 88 blend_equation_advanced cases natively while no guest could
   ask for the feature at all -- 585 claimable extensions became 586 when this
   appeared. That gap is invisible to every native gate, because the native
   driver is reached through the system libGL and not through this
   marshaller.

   glBlendBarrierKHR and glBlendBarrierNV both alias this in the registry, so
   gl-aliases-generated.inc emits them from the base defined here. */
void glBlendBarrier(void) { CMD0(GEXGL_BLEND_BARRIER); }

/* ARB_gl_spirv: hand a shader to GL as a SPIR-V module rather than as GLSL.
   Mesa does the whole job -- glSpecializeShader runs spirv_to_nir and what
   reaches the driver is the NIR it already takes -- so the only thing between
   a guest and working SPIR-V was these two entry points. Without them
   gen_gl_extensions.py withheld GL_ARB_gl_spirv, and gl44-spirv-probe ran its
   three cases natively and *zero* in a browser: it asks for the extension,
   does not find it, and skips. A probe reporting cases=0 is the quiet shape
   of this whole class of gap.

   Both are hand-written for the reason glBlendBarrier is: they are GL 4.6 and
   4.1 core entry points that the 4.4 manifest does not carry, and the
   generated opcodes are indices into the sorted 4.4 command list, so a new
   name cannot join it without renumbering nearly the whole ABI.

   Arrays travel as blobs -- a length word then the bytes, padded to four --
   because that is what the host's reader already understands. */
void glShaderBinary(int count, const unsigned *shaders, unsigned format,
                    const void *binary, int length) {
    /* Latched here, not forwarded: this entry point is one the guest answers
       for itself -- it joins the caller's fragments, or handles the call
       entirely locally -- so the host never sees the count the application
       passed and could not raise the error for it. Safe to answer locally
       only because the reference agrees on which error: native Mesa raises
       GL_INVALID_VALUE for all of these, unlike the multi-bind entry points
       where it answers GL_INVALID_OPERATION against the specification. */
    if (count < 0 || length < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    if (count > 0 && !shaders) return;
    if (length > 0 && !binary) return;
    size_t names = (size_t)count * 4u;
    size_t blob = (size_t)length;
    size_t tail;
    /* two blobs: a length word and padded payload each */
    if (!gexgl_size_add(4u + names, 4u + ((blob + 3u) & ~(size_t)3u), &tail))
        return;
    uint32_t *p = begin(GEXGL_SHADER_BINARY, 2, tail);
    if (!p) return;
    p[0] = (uint32_t)count;
    p[1] = format;
    p[2] = (uint32_t)names;
    if (names) memcpy(p + 3, shaders, names);
    uint32_t *at = p + 3 + count;
    at[0] = (uint32_t)blob;
    if (blob) {
        memcpy(at + 1, binary, blob);
        size_t padded = (blob + 3u) & ~(size_t)3u;
        if (padded > blob) memset((uint8_t *)(at + 1) + blob, 0, padded - blob);
    }
}

void glSpecializeShader(unsigned shader, const char *entry, unsigned count,
                        const unsigned *index, const unsigned *value) {
    if (count > 0 && (!index || !value)) return;
    size_t entry_len = entry ? strlen(entry) : 0;
    size_t padded_entry = (entry_len + 3u) & ~(size_t)3u;
    size_t words = (size_t)count * 4u;
    size_t tail;
    if (!gexgl_size_add(4u + padded_entry, (4u + words) * 2u, &tail)) return;
    uint32_t *p = begin(GEXGL_SPECIALIZE_SHADER, 2, tail);
    if (!p) return;
    p[0] = shader;
    p[1] = count;
    p[2] = (uint32_t)entry_len;
    if (entry_len) {
        memcpy(p + 3, entry, entry_len);
        if (padded_entry > entry_len)
            memset((uint8_t *)(p + 3) + entry_len, 0, padded_entry - entry_len);
    }
    uint32_t *at = p + 3 + padded_entry / 4u;
    at[0] = (uint32_t)words;
    if (words) memcpy(at + 1, index, words);
    at += 1 + count;
    at[0] = (uint32_t)words;
    if (words) memcpy(at + 1, value, words);
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

/* The error latch lives with glGetError further down; mapping is emulated
   here, so these need it before then.

   GEX_GL_LOG_ERRORS=1 names the call that latched one. The guest raises GL's
   errors for everything it answers on this side, and a spurious one is
   otherwise invisible: the application sees the code with nothing to say
   where it came from, and the CTS reports it against whichever case happened
   to check next. */
/* Declared at the top of the file: every entry point that latches an error
   needs these, and half of them appear above this point. */
/* Both halves of the persistent-mapping traffic, defined with the draw path
   further down and called from glFinish above it. */
static void gexgl_flush_coherent_maps(void);
static void gexgl_refresh_persistent_reads(void);

void glFinish(void) {
    CMD0(GEXGL_FINISH);
    flush();
    (void)reply_u32();
    /* After the host has finished, whatever it wrote through a persistent
       mapping is visible; the shadow has to catch up before the application
       reads it. */
    gexgl_refresh_persistent_reads();
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

/* The same, for GL 4.5's glCreate* family, whose texture and query forms take
   a target ahead of the count. Names are still chosen here and simply told to
   the host, so there is no reply to wait for. */
static void gen_named_objects_target(unsigned opcode, unsigned target, int n,
                                     unsigned *out) {
    if (n < 0) {
        CMD2(opcode, target, n);
        return;
    }
    uint32_t *p = begin(opcode, 2 + (size_t)n, 0);
    if (!p) return;
    p[0] = target;
    p[1] = (uint32_t)n;
    for (int i = 0; i < n; ++i) {
        out[i] = take_name();
        p[2 + i] = out[i];
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
    /* The flags glBufferStorage was called with, as the application sees
       them. The host is given one more -- see the note in
       tools/gen_gl44_transport.py -- so GL_BUFFER_STORAGE_FLAGS has to be
       answered from here rather than forwarded. */
    unsigned storage_flags;
    int has_storage_flags;
    /* What the host was last told this mapping holds, for the coherent case
       below. Allocated only for a mapping that needs it. */
    void *map_sent;
    struct buffer_object_state *next;
};
/* GL 4.6 has fifteen generic buffer targets (ARB_indirect_parameters'
   GL_PARAMETER_BUFFER is the fifteenth). A table that cannot hold them all
   made buffer_target() return NULL, and every caller read that as "nothing
   bound" -- a silent drop, so say so instead. */
static struct buffer_target_state buffer_targets[24];
static struct buffer_object_state *buffer_objects;

static struct buffer_target_state *buffer_target(unsigned target) {
    struct buffer_target_state *free_slot = NULL;
    for (size_t i = 0; i < sizeof(buffer_targets) / sizeof(buffer_targets[0]); i++) {
        if (buffer_targets[i].target == target) return &buffer_targets[i];
        if (!buffer_targets[i].target && !free_slot) free_slot = &buffer_targets[i];
    }
    if (free_slot) {
        free_slot->target = target;
        return free_slot;
    }
    fprintf(stderr, "gexgl: buffer target table full, dropping target 0x%x\n",
            target);
    return NULL;
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

/* An indexed bind also sets the generic binding point of the same target --
   glBindBufferBase(target, i, b) is defined to do what glBindBuffer(target, b)
   does as well. Guest-side buffer state (storage flags, size, mappings) is
   keyed off that generic point, so without this a glBufferStorage issued
   after glBindBufferBase recorded nothing at all: GL_BUFFER_STORAGE_FLAGS
   answered 0, and glMapBufferRange on the target returned NULL. */
static void bind_generic_buffer(unsigned target, unsigned name) {
    struct buffer_target_state *s = buffer_target(target);
    if (s) s->name = name;
    if (name) (void)buffer_object(name, 1);
}

/* Mapping is emulated on this side: the host is never told a buffer is
   mapped, so it cannot raise any of mapping's errors and cannot answer any of
   mapping's state queries. Both belong here, with the bookkeeping.

   Returns the GL error a map of this range would raise, or 0. Shared by the
   bound and named spellings so the two cannot drift -- the same split is what
   left GL_BUFFER_STORAGE_FLAGS answered for one form and forwarded for the
   other. */
static unsigned map_range_error(const struct buffer_object_state *object,
                                long offset, long length, unsigned access) {
    const unsigned defined = 0x0001u | 0x0002u | 0x0004u | 0x0008u |
                             0x0010u | 0x0020u | 0x0040u | 0x0080u;
    /* The split between the two error codes is the spec's, not intuition's:
       everything about the *range* is GL_INVALID_VALUE and everything about
       the *access bits* is GL_INVALID_OPERATION, including the empty length
       and the "neither read nor write" case. Reporting INVALID_VALUE for the
       latter is what KHR-GL44.buffer_storage.errors caught. */
    if (!object) return 0x0502u;                    /* GL_INVALID_OPERATION */
    if (offset < 0 || length < 0) return 0x0501u;   /* GL_INVALID_VALUE */
    if ((size_t)offset > object->size ||
        (size_t)length > object->size - (size_t)offset)
        return 0x0501u;
    if (access & ~defined) return 0x0501u;
    if (length == 0) return 0x0502u;
    if (object->map) return 0x0502u;                /* already mapped */
    if (!(access & (0x0001u | 0x0002u))) return 0x0502u;  /* neither R nor W */
    /* Reading forbids the three bits that say the old contents do not
       matter, and an explicit flush needs something to flush. */
    if ((access & 0x0001u) && (access & (0x0004u | 0x0008u | 0x0020u)))
        return 0x0502u;
    if ((access & 0x0010u) && !(access & 0x0002u)) return 0x0502u;
    if (!object->has_storage_flags) return 0;  /* mutable: flags do not gate */
    /* An immutable buffer may only be mapped with what its storage allows.
       The host's copy of the flags has GL_DYNAMIC_STORAGE_BIT added so a
       write-back is accepted at all, so only this side can make this check. */
    {
        const unsigned gated = 0x0001u | 0x0002u | 0x0040u | 0x0080u;
        if ((access & gated) & ~object->storage_flags) return 0x0502u;
    }
    return 0;
}

/* The GL error a glBufferSubData into this range would raise, or 0. Two rules,
   both of which only this side can apply:

   - 6.2: an error on any part of a range that is mapped, unless that mapping
     set GL_MAP_PERSISTENT_BIT. The host is never told about the mapping.
   - an immutable buffer whose storage flags lack GL_DYNAMIC_STORAGE_BIT
     cannot be written this way at all. The host's copy of the flags always
     has that bit added -- it is what makes the mapping write-back work -- so
     the host will never raise this. */
static unsigned sub_data_error(const struct buffer_object_state *object,
                               long offset, long size) {
    if (!object) return 0;
    if (object->has_storage_flags && !(object->storage_flags & 0x0100u))
        return 0x0502u;                           /* GL_INVALID_OPERATION */
    if (!object->map) return 0;
    if (object->map_access & 0x0040u) return 0;   /* GL_MAP_PERSISTENT_BIT */
    if ((size_t)offset < object->map_offset + object->map_length &&
        object->map_offset < (size_t)offset + (size_t)size)
        return 0x0502u;
    return 0;
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
    if (size < 0) { GEXGL_SET_ERROR(0x0501u); return; }
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

/* The write itself, with none of GL's checks. The driver's own write-back of
   a mapping goes through here: the flags the application asked for need carry
   neither GL_DYNAMIC_STORAGE_BIT nor anything else that would let it make this
   call, and the checks below would reject the driver's own traffic. */
static void buffer_sub_data_raw(unsigned target, long offset, long size,
                                const void *data) {
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

void glBufferSubData(unsigned target, long offset, long size, const void *data) {
    if (offset < 0 || size < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    if (size && !data) return;
    {
        const unsigned bad =
            sub_data_error(buffer_object(bound_buffer(target), 0), offset, size);
        if (bad) { GEXGL_SET_ERROR(bad); return; }
    }
    buffer_sub_data_raw(target, offset, size, data);
}

/* The named forms of the three above. GL 4.5 core, and the ones that make
   direct state access usable rather than merely present: the DSA object model
   is there now, but a buffer cannot be filled without these, so an engine
   taking the 4.5 path still stopped at its first vertex upload.

   Hand-written beside their bound twins rather than generated, because those
   twins are hand-written too -- the guest keeps a shadow of each buffer's size
   and storage flags, and that bookkeeping is what glMapBufferRange and the
   persistent-mapping path read. A generated version would send the bytes and
   silently skip the shadow.

   The only difference from the bound forms is where the buffer comes from:
   named here, `bound_buffer(target)` there. */
void glNamedBufferData(unsigned buffer, long size, const void *data,
                       unsigned usage) {
    if (size < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    size_t len = (size_t)size;
    struct buffer_object_state *object = buffer_object(buffer, 1);
    if (object) object->size = len;
    size_t blob_len = data ? len : 0;
    uint32_t *p = begin(GEXGL_NAMED_BUFFER_DATA, 5, 4 + blob_len);
    if (!p) return;
    p[0] = buffer;
    p[1] = usage;
    p[2] = (uint32_t)(uint64_t)size;
    p[3] = (uint32_t)((uint64_t)size >> 32);
    p[4] = data != NULL;
    put_blob(p + 5, data, blob_len);
}

static void named_buffer_sub_data_raw(unsigned buffer, long offset, long size,
                                      const void *data) {
    if (offset < 0 || size < 0 || (size && !data)) return;
    size_t len = (size_t)size;
    uint32_t *p = begin(GEXGL_NAMED_BUFFER_SUB_DATA, 5, 4 + len);
    if (!p) return;
    p[0] = buffer;
    p[1] = (uint32_t)(uint64_t)offset;
    p[2] = (uint32_t)((uint64_t)offset >> 32);
    p[3] = (uint32_t)(uint64_t)size;
    p[4] = (uint32_t)((uint64_t)size >> 32);
    put_blob(p + 5, data, len);
}

void glNamedBufferSubData(unsigned buffer, long offset, long size,
                          const void *data) {
    if (offset < 0 || size < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    if (size && !data) return;
    {
        const unsigned bad =
            sub_data_error(buffer_object(buffer, 0), offset, size);
        if (bad) { GEXGL_SET_ERROR(bad); return; }
    }
    named_buffer_sub_data_raw(buffer, offset, size, data);
}

void glNamedBufferStorage(unsigned buffer, long size, const void *data,
                          unsigned flags) {
    /* A refusal with no error is worse than no refusal: the application asks
       glGetError, is told nothing went wrong, and uses a buffer that has no
       storage. */
    if (size < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    size_t blob_len = data ? (size_t)size : 0;
    struct buffer_object_state *object = buffer_object(buffer, 1);
    if (object) {
        object->size = (size_t)size;
        object->storage_flags = flags;
        object->has_storage_flags = 1;
    }
    uint32_t *p = begin(GEXGL_NAMED_BUFFER_STORAGE, 5, 4 + blob_len);
    if (!p) return;
    const uint64_t bytes = (uint64_t)size;
    p[0] = buffer;
    p[1] = (uint32_t)bytes;
    p[2] = (uint32_t)(bytes >> 32);
    /* GL_DYNAMIC_STORAGE_BIT, as the generated glBufferStorage adds: the host
       must accept a later glNamedBufferSubData whatever the guest asked for. */
    p[3] = flags | 0x0100u;
    p[4] = data != NULL;
    put_blob(p + 5, data, blob_len);
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
    const unsigned bad = map_range_error(object, offset, length, access);
    if (bad) { GEXGL_SET_ERROR(bad); return NULL; }
    object->map_length = (size_t)length;
    object->map_offset = (size_t)offset;
    object->map_access = access;
    object->map = calloc(1, object->map_length ? object->map_length : 1);
    /* GL_MAP_READ_BIT, or any persistent mapping. A write-only mapping is
       normally handed a zeroed block and never asks the host what the buffer
       already holds -- that is what keeps the streaming case cheap, and it is
       right, because such a mapping is written whole and uploaded at unmap.
       A persistent one is not: the coherent flush uploads whatever differs
       from what it last sent, so a shadow that starts out zeroed uploads
       zeros over data the application never touched. glBufferStorage's own
       initial contents went that way, and the compute shader reading that
       buffer saw nothing but zeroes. One fetch, on a mapping that lives for
       the program's lifetime. */
    const int fetched =
        object->map && ((access & 1u) || (access & 0x40u)) && object->map_length;
    if (fetched)
        glGetBufferSubData(target, offset, length, object->map);
    /* The shadow starts life here, beside the fetch, holding exactly what the
       host holds. It cannot be built lazily at the first flush: by then the
       application has already written through the mapping, so filling it from
       the mapping would adopt that write as already-sent and the bytes would
       never travel. Nor can the first flush simply send everything, which is
       what it used to do -- for a buffer the host has written since the map,
       that overwrites the host's data with the guest's older copy. Both of
       those were live bugs on the same afternoon, in opposite directions;
       having the shadow exist from the start is what makes the diff mean
       "what the application has changed since the host last agreed". */
    if (fetched) {
        free(object->map_sent);
        object->map_sent = malloc(object->map_length);
        if (object->map_sent)
            memcpy(object->map_sent, object->map, object->map_length);
    }
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
    if (object->map_access & 2u)
        buffer_sub_data_raw(target, (long)object->map_offset,
                            (long)object->map_length, object->map);
    free(object->map);
    free(object->map_sent);
    object->map = NULL;
    object->map_sent = NULL;
    object->map_offset = object->map_length = 0;
    return 1;
}

/* Record a span of a mapping as published, so the coherent and barrier
   diffs do not send it a second time.
 *
 * The explicit flushes below send bytes without touching the shadow, which
 * leaves it claiming the mapping still holds what it held when it was mapped.
 * Nothing minded while the shadow was only consulted per draw for coherent
 * mappings -- re-sending identical bytes is wasteful, not wrong. It became
 * wrong once a *barrier* flushed non-coherent mappings too: by then the GPU
 * may have written the buffer, and re-sending the application's older bytes
 * over its output is a lost result rather than a wasted upload. That is
 * KHR-GL44.buffer_storage.map_persistent_draw, whose GPU-incremented atomic
 * counter came back holding whatever the application had last written. */
static void gexgl_record_map_sent(struct buffer_object_state *object,
                                  size_t offset, size_t length) {
    if (!object->map || !object->map_length) return;
    if (offset > object->map_length ||
        length > object->map_length - offset) return;
    /* No shadow means the map-time allocation failed. Building one here would
       have to invent a value for the rest of the mapping, and either choice is
       a bug: the application's unpublished writes outside this span would be
       adopted as sent, or the host's data would be queued to be overwritten.
       Leave it absent and let the flush path's conservative fallback send
       everything, which loses nothing. */
    if (!object->map_sent) return;
    memcpy((unsigned char *)object->map_sent + offset,
           (const unsigned char *)object->map + offset, length);
}

void glFlushMappedBufferRange(unsigned target, long offset, long length) {
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 0);
    if (!object || !object->map || offset < 0 || length < 0 ||
        (size_t)offset > object->map_length ||
        (size_t)length > object->map_length - (size_t)offset) return;
    buffer_sub_data_raw(target, (long)(object->map_offset + (size_t)offset), length,
                    (uint8_t *)object->map + offset);
    gexgl_record_map_sent(object, (size_t)offset, (size_t)length);
}

/* Declared here, defined with the other round-trip calls further down -- the
   same arrangement glGetBufferSubData has, and for the same reason: the
   mapping code below needs it and the definition belongs beside its siblings.
   Without this the compiler assumed int(), which is a different calling
   convention for the pointer argument and would have passed it wrongly on any
   ABI where that matters. */
void glGetNamedBufferSubData(unsigned buffer, long offset, long size,
                             void *data);

/* The named mapping family. GL 4.5 core, and entirely guest-side, like the
   bound forms directly above: a GL mapping here is a shadow buffer this
   library owns, filled by a readback when the caller asks to read and sent
   back when it unmaps. Nothing is mapped on the host at all.

   That is why these could not be generated. glFlushMappedNamedBufferRange in
   particular *looks* like three scalars and is not -- the bytes live in the
   shadow, so the flush has to carry them. Generated as a scalar it sent the
   offset and length alone, the data never travelled, and nothing reported an
   error; it is written out here for the same reason its bound twin is. */
void *glMapNamedBufferRange(unsigned buffer, long offset, long length,
                            unsigned access) {
    struct buffer_object_state *object = buffer_object(buffer, 0);
    const unsigned bad = map_range_error(object, offset, length, access);
    if (bad) { GEXGL_SET_ERROR(bad); return NULL; }
    object->map_length = (size_t)length;
    object->map_offset = (size_t)offset;
    object->map_access = access;
    object->map = calloc(1, object->map_length ? object->map_length : 1);
    /* GL_MAP_READ_BIT pays for the round trip, and so does any persistent
       mapping -- see the note in glMapBufferRange. A write-only,
       non-persistent mapping still does not: that would double the cost of
       the streaming case this exists to serve. */
    const int fetched =
        object->map && ((access & 1u) || (access & 0x40u)) && object->map_length;
    if (fetched)
        glGetNamedBufferSubData(buffer, offset, length, object->map);
    /* The shadow starts life here, beside the fetch, holding exactly what the
       host holds. It cannot be built lazily at the first flush: by then the
       application has already written through the mapping, so filling it from
       the mapping would adopt that write as already-sent and the bytes would
       never travel. Nor can the first flush simply send everything, which is
       what it used to do -- for a buffer the host has written since the map,
       that overwrites the host's data with the guest's older copy. Both of
       those were live bugs on the same afternoon, in opposite directions;
       having the shadow exist from the start is what makes the diff mean
       "what the application has changed since the host last agreed". */
    if (fetched) {
        free(object->map_sent);
        object->map_sent = malloc(object->map_length);
        if (object->map_sent)
            memcpy(object->map_sent, object->map, object->map_length);
    }
    return object->map;
}

void *glMapNamedBuffer(unsigned buffer, unsigned access) {
    struct buffer_object_state *object = buffer_object(buffer, 0);
    const unsigned bits = access == 0x88B8u ? 1u :   /* GL_READ_ONLY */
                          access == 0x88B9u ? 2u : 3u; /* WRITE / READ_WRITE */
    return object ? glMapNamedBufferRange(buffer, 0, (long)object->size, bits)
                  : NULL;
}

unsigned char glUnmapNamedBuffer(unsigned buffer) {
    struct buffer_object_state *object = buffer_object(buffer, 0);
    if (!object || !object->map) return 0;
    if (object->map_access & 2u)
        named_buffer_sub_data_raw(buffer, (long)object->map_offset,
                             (long)object->map_length, object->map);
    free(object->map);
    free(object->map_sent);
    object->map = NULL;
    object->map_sent = NULL;
    object->map_offset = object->map_length = 0;
    return 1;
}

void glFlushMappedNamedBufferRange(unsigned buffer, long offset, long length) {
    struct buffer_object_state *object = buffer_object(buffer, 0);
    if (!object || !object->map || offset < 0 || length < 0 ||
        (size_t)offset > object->map_length ||
        (size_t)length > object->map_length - (size_t)offset) return;
    named_buffer_sub_data_raw(buffer, (long)(object->map_offset + (size_t)offset),
                         length, (uint8_t *)object->map + offset);
    gexgl_record_map_sent(object, (size_t)offset, (size_t)length);
}

void glGetNamedBufferPointerv(unsigned buffer, unsigned pname, void **params) {
    if (!params) return;
    struct buffer_object_state *object = buffer_object(buffer, 0);
    *params = (pname == 0x88BDu /* GL_BUFFER_MAP_POINTER */ && object)
        ? object->map : NULL;
}

/* The last two DSA commands, both shapes the generator cannot express.
 *
 * glVertexArrayVertexBuffers takes three parallel arrays -- names, offsets and
 * strides -- where every policy here carries at most one. They travel as one
 * blob of count*(4+8+4) bytes rather than three, so a truncated packet cannot
 * leave them disagreeing about how many entries arrived.
 *
 * glInvalidateNamedFramebufferSubData puts its attachment array ahead of
 * x/y/width/height, and ARRAY_COMMANDS requires the array last; the generator
 * refuses it rather than emitting something wrong, which is why it is here. */
void glVertexArrayVertexBuffers(unsigned vaobj, unsigned first, int count,
                                const unsigned *buffers, const long *offsets,
                                const int *strides) {
    /* A negative count belongs to GL, and only the host has a GL context to
       raise the error in. Returning here left the application with no error
       and no effect. Forward it; the guards below are ours and apply to a
       count that is positive. */
    /* GL says a NULL buffers array unbinds the range; offsets and strides are
       then ignored. Anything else with a missing array is a caller error. */
    if (count > 0 && buffers && (!offsets || !strides)) return;
    const size_t entries = count > 0 ? (size_t)count : 0u;
    if (entries > (UINT32_MAX - 8u) / 16u) return;
    const size_t blob = entries * 16u;
    uint32_t *p = begin(GEXGL_VERTEX_ARRAY_VERTEX_BUFFERS, 4, 4 + blob);
    if (!p) return;
    p[0] = vaobj;
    p[1] = first;
    p[2] = (uint32_t)count;
    p[3] = buffers != NULL;
    p[4] = (uint32_t)blob;
    uint32_t *at = p + 5;
    for (size_t i = 0; i < entries; ++i) {
        const uint64_t off = buffers ? (uint64_t)offsets[i] : 0u;
        at[0] = buffers ? buffers[i] : 0u;
        at[1] = (uint32_t)off;
        at[2] = (uint32_t)(off >> 32);
        at[3] = buffers ? (uint32_t)strides[i] : 0u;
        at += 4;
    }
}

void glInvalidateNamedFramebufferSubData(unsigned framebuffer, int numAttachments,
                                         const unsigned *attachments, int x, int y,
                                         int width, int height) {
    /* A negative count belongs to GL, and only the host has a GL context to
       raise the error in. Returning here left the application with no error
       and no effect. Forward it; the guards below are ours and apply to a
       count that is positive. */
    if (numAttachments > 0 && !attachments) return;
    const size_t entries = numAttachments > 0 ? (size_t)numAttachments : 0u;
    if (entries > (UINT32_MAX - 8u) / 4u) return;
    const size_t blob = entries * 4u;
    uint32_t *p = begin(GEXGL_INVALIDATE_NAMED_FRAMEBUFFER_SUB_DATA, 6, 4 + blob);
    if (!p) return;
    p[0] = framebuffer;
    p[1] = (uint32_t)numAttachments;
    p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)width; p[5] = (uint32_t)height;
    p[6] = (uint32_t)blob;
    if (blob) memcpy(p + 7, attachments, blob);
}

void glBindBufferBase(unsigned target, unsigned index, unsigned buffer) {
    bind_generic_buffer(target, buffer);
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
    if (gexgl_trace_errors())
        fprintf(stderr, "gexgl: glCreateShader(0x%x) -> %u\n", type, name);
    CMD2(GEXGL_CREATE_SHADER, name, type);
    return name;
}

void glShaderSource(unsigned shader, int count, const char *const *strings,
                    const int *lengths) {
    /* GL takes an array of fragments; the host wants one string, so they are
       joined here rather than sending the array. */
    /* Latched here, not forwarded: this entry point is one the guest answers
       for itself -- it joins the caller's fragments, or handles the call
       entirely locally -- so the host never sees the count the application
       passed and could not raise the error for it. Safe to answer locally
       only because the reference agrees on which error: native Mesa raises
       GL_INVALID_VALUE for all of these, unlike the multi-bind entry points
       where it answers GL_INVALID_OPERATION against the specification. */
    if (count < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    if (count > 0 && !strings) return;
    size_t total = 0;
    for (int i = 0; i < count; i++) {
        size_t n;
        if (!strings[i]) return;
        n = lengths && lengths[i] >= 0 ? (size_t)lengths[i] : strlen(strings[i]);
        if (!gexgl_size_add(total, n, &total)) return;
    }
    size_t tail;
    if (total > UINT32_MAX || !gexgl_size_add(4, total, &tail)) return;
    if (gexgl_trace_errors())
        fprintf(stderr, "gexgl: glShaderSource(%u, %zu bytes)\n", shader,
                total);
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
    if (!out) return;
    /* GL_COMPUTE_WORK_GROUP_SIZE is the one program property that is three
       integers rather than one. Transporting a single word left the second
       and third untouched, so every caller read the local size as (x, 0, 0)
       -- and dEQP checks it in the *setup* of its compute cases, which is why
       KHR-GL44.compute_shader.simple-compute, one-work-group and
       dispatch-indirect failed before their bodies ran. */
    const int words = pname == 0x8267u ? 3 : 1;
    CMD2(GEXGL_GET_PROGRAM_IV, program, pname);
    flush();
    for (int i = 0; i < words; ++i) out[i] = (int)reply_u32();
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

/* Generated further down, used just below. Without the declaration the
   compiler assumes int() and the third argument goes through a different
   convention -- the same trap glGetNamedBufferSubData fell into earlier. */
void glProgramParameteri(unsigned program, unsigned pname, int value);

/* ARB_separate_shader_objects' last two, ARB_get_program_binary, and two
   odds. None of these was reachable before; each is the only thing its
   extension was waiting on.

   glCreateShaderProgramv needs no opcode at all. The specification defines it
   as a sequence of calls this library already has, so it is written as that
   sequence -- create, compile, mark separable, attach, link, detach, delete --
   and the host sees ordinary commands. Composing it here also means the guest
   keeps its own program and shader names, which is what every other call
   expects. */
unsigned glCreateShaderProgramv(unsigned type, int count,
                                const char *const *strings) {
    if (count < 0) return 0;
    const unsigned shader = glCreateShader(type);
    if (!shader) return 0;
    glShaderSource(shader, count, strings, NULL);
    glCompileShader(shader);
    const unsigned program = glCreateProgram();
    if (!program) { glDeleteShader(shader); return 0; }
    int compiled = 0;
    glGetShaderiv(shader, 0x8B81u /* GL_COMPILE_STATUS */, &compiled);
    if (compiled) {
        /* GL_PROGRAM_SEPARABLE, and it must be set *before* the link or the
           program cannot be used in a pipeline, which is the entire point. */
        glProgramParameteri(program, 0x8258u, 1);
        glAttachShader(program, shader);
        glLinkProgram(program);
        glDetachShader(program, shader);
    }
    glDeleteShader(shader);
    return program;
}

void glGetProgramPipelineInfoLog(unsigned pipeline, int bufSize, int *length,
                                 char *infoLog) {
    get_log(GEXGL_GET_PROGRAM_PIPELINE_INFO_LOG, pipeline, bufSize, length,
            infoLog);
}

/* A program binary is opaque: the guest neither makes nor reads it, it only
   carries the bytes and the format between the application and the host. What
   it must not do is hand back a truncated one -- an application stores what it
   is given and feeds it back later, so a short binary is a cache that fails at
   the next run rather than at this one. */
void glGetProgramBinary(unsigned program, int bufSize, int *length,
                        unsigned *binaryFormat, void *binary) {
    if (length) *length = 0;
    /* Latched here, not forwarded: this entry point is one the guest answers
       for itself -- it joins the caller's fragments, or handles the call
       entirely locally -- so the host never sees the count the application
       passed and could not raise the error for it. Safe to answer locally
       only because the reference agrees on which error: native Mesa raises
       GL_INVALID_VALUE for all of these, unlike the multi-bind entry points
       where it answers GL_INVALID_OPERATION against the specification. */
    if (bufSize < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    uint32_t *p = begin(GEXGL_GET_PROGRAM_BINARY, 2, 0);
    if (!p) return;
    p[0] = program;
    p[1] = (uint32_t)bufSize;
    flush();
    const unsigned format = reply_u32();
    uint32_t n = 0;
    const uint8_t *reply = reply_blob(&n);
    if (binaryFormat) *binaryFormat = format;
    if (!reply || (int)n > bufSize) return;
    if (binary && n) memcpy(binary, reply, n);
    if (length) *length = (int)n;
}

void glProgramBinary(unsigned program, unsigned binaryFormat,
                     const void *binary, int length) {
    if (length < 0 || (length && !binary)) return;
    uint32_t *p = begin(GEXGL_PROGRAM_BINARY, 2, 4 + (size_t)length);
    if (!p) return;
    p[0] = program;
    p[1] = binaryFormat;
    put_blob(p + 2, binary, (size_t)length);
}

/* ARB_invalidate_subdata's remaining call. Like its DSA twin the attachment
   array sits ahead of x/y/width/height, which no policy here can express. */
void glInvalidateSubFramebuffer(unsigned target, int numAttachments,
                                const unsigned *attachments, int x, int y,
                                int width, int height) {
    /* A negative count belongs to GL, and only the host has a GL context to
       raise the error in. Returning here left the application with no error
       and no effect. Forward it; the guards below are ours and apply to a
       count that is positive. */
    if (numAttachments > 0 && !attachments) return;
    const size_t entries = numAttachments > 0 ? (size_t)numAttachments : 0u;
    if (entries > (UINT32_MAX - 8u) / 4u) return;
    const size_t blob = entries * 4u;
    uint32_t *p = begin(GEXGL_INVALIDATE_SUB_FRAMEBUFFER, 6, 4 + blob);
    if (!p) return;
    p[0] = target;
    p[1] = (uint32_t)numAttachments;
    p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)width; p[5] = (uint32_t)height;
    p[6] = (uint32_t)blob;
    if (blob) memcpy(p + 7, attachments, blob);
}

/* KHR_parallel_shader_compile. A hint, and the specification says so: an
   implementation may ignore it entirely. This one does, rather than adding a
   round trip to tell the host a number it would also ignore -- the guest
   compiles nothing itself, so there are no threads here to bound. */
void glMaxShaderCompilerThreadsKHR(unsigned count) { (void)count; }

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

/* The subroutine queries, and the last of GL 4.6's core command set to arrive.
 *
 * They are get_indexed_name with a shader stage in front -- GL keeps a
 * separate subroutine namespace per stage, so the stage is part of the key
 * rather than context. glGetActiveSubroutineUniformiv answers a variable
 * number of integers, GL_COMPATIBLE_SUBROUTINES most of all, so its reply is
 * sized by the host and read with reply_ints below. */
static void get_stage_indexed_name(uint32_t opcode, unsigned program,
                                   unsigned shadertype, unsigned index,
                                   int max, int *length, char *name) {
    uint32_t *p = begin(opcode, 3, 0);
    if (!p) return;
    p[0] = program; p[1] = shadertype; p[2] = index;
    flush();
    uint32_t n = 0;
    const uint8_t *reply = reply_blob(&n);
    if ((int)n > max - 1) n = max > 0 ? (uint32_t)max - 1 : 0;
    if (reply && name && max > 0) {
        memcpy(name, reply, n);
        name[n] = 0;
    }
    if (length) *length = (int)n;
}

void glGetActiveSubroutineName(unsigned program, unsigned shadertype,
                               unsigned index, int bufSize, int *length,
                               char *name) {
    get_stage_indexed_name(GEXGL_GET_ACTIVE_SUBROUTINE_NAME, program,
                           shadertype, index, bufSize, length, name);
}

void glGetActiveSubroutineUniformName(unsigned program, unsigned shadertype,
                                      unsigned index, int bufSize, int *length,
                                      char *name) {
    get_stage_indexed_name(GEXGL_GET_ACTIVE_SUBROUTINE_UNIFORM_NAME, program,
                           shadertype, index, bufSize, length, name);
}

/* However many integers the query yields; the host knows the count from the
   pname, so the reply carries it. */
static void reply_ints(int *params) {
    uint32_t n = 0;
    const uint8_t *p = reply_blob(&n);
    if (params && p && n)
        memcpy(params, p, n - (n % 4));
}

void glGetActiveSubroutineUniformiv(unsigned program, unsigned shadertype,
                                    unsigned index, unsigned pname,
                                    int *values) {
    uint32_t *p = begin(GEXGL_GET_ACTIVE_SUBROUTINE_UNIFORM_IV, 4, 0);
    if (!p) return;
    p[0] = program; p[1] = shadertype; p[2] = index; p[3] = pname;
    flush();
    reply_ints(values);
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
    /* Latched here, not forwarded: this entry point is one the guest answers
       for itself -- it joins the caller's fragments, or handles the call
       entirely locally -- so the host never sees the count the application
       passed and could not raise the error for it. Safe to answer locally
       only because the reference agrees on which error: native Mesa raises
       GL_INVALID_VALUE for all of these, unlike the multi-bind entry points
       where it answers GL_INVALID_OPERATION against the specification. */
    if (count < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    if (count > 0 && !names) return;
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

/* ------------------------------------------------------------- KHR_debug */
/*
 * Entirely on this side of the ring, and it has to be: the callback an
 * application installs is a pointer into *its* address space, so nothing on
 * the host could ever call it. GL allows an implementation to produce no
 * messages of its own, which is what this one does -- what it must get right
 * is that the calls exist, that a message the application inserts comes back,
 * and that a label it sets can be read again.
 *
 * That matters more than it sounds. An engine that sees GL 4.3 installs a
 * debug callback during start-up, and glDebugMessageCallback resolving to NULL
 * is a crash before the first frame rather than a missing diagnostic.
 */
#define GEXGL_DEBUG_MESSAGE_MAX 1024
#define GEXGL_DEBUG_LOG_DEPTH 64
#define GEXGL_DEBUG_GROUP_DEPTH 64
#define GEXGL_DEBUG_LABEL_MAX 256

typedef void (*gexgl_debug_proc)(unsigned source, unsigned type, unsigned id,
                                 unsigned severity, int length,
                                 const char *message, const void *user);

struct gexgl_debug_record {
    unsigned source, type, id, severity;
    int length;
    char text[GEXGL_DEBUG_MESSAGE_MAX];
};

struct gexgl_debug_label {
    unsigned identifier;
    unsigned name;
    const void *pointer;
    char text[GEXGL_DEBUG_LABEL_MAX];
};

static struct {
    gexgl_debug_proc callback;
    const void *user;
    int output_enabled;
    struct gexgl_debug_record log[GEXGL_DEBUG_LOG_DEPTH];
    unsigned logged;              /* how many of `log` are live */
    struct { unsigned source, id; char text[GEXGL_DEBUG_MESSAGE_MAX]; }
        group[GEXGL_DEBUG_GROUP_DEPTH];
    unsigned depth;
    struct gexgl_debug_label *labels;
    size_t label_count;
} debug_state;

/* GL's "is this message wanted" test, reduced to what this implementation can
   actually distinguish. glDebugMessageControl's per-id filtering is accepted
   and ignored: nothing here generates ids of its own, so the only messages it
   could suppress are the application's own inserts, and GL permits an
   implementation to keep those. */
static int gexgl_debug_wanted(void) {
    return debug_state.output_enabled;
}

static void gexgl_debug_emit(unsigned source, unsigned type, unsigned id,
                             unsigned severity, int length, const char *text) {
    if (!gexgl_debug_wanted() || !text) return;
    size_t n = length < 0 ? strlen(text) : (size_t)length;
    if (n >= GEXGL_DEBUG_MESSAGE_MAX) n = GEXGL_DEBUG_MESSAGE_MAX - 1;
    if (debug_state.callback) {
        /* Straight through, as GL_DEBUG_OUTPUT_SYNCHRONOUS demands and as an
           application single-stepping in a debugger expects. A copy, because
           the callback is handed a NUL-terminated string and the caller's may
           not be. */
        char copy[GEXGL_DEBUG_MESSAGE_MAX];
        memcpy(copy, text, n);
        copy[n] = '\0';
        debug_state.callback(source, type, id, severity, (int)n, copy,
                             debug_state.user);
        return;
    }
    /* No callback: the message queues for glGetDebugMessageLog. GL drops the
       oldest when the log is full, so this does too. */
    if (debug_state.logged == GEXGL_DEBUG_LOG_DEPTH) {
        memmove(&debug_state.log[0], &debug_state.log[1],
                sizeof(debug_state.log[0]) * (GEXGL_DEBUG_LOG_DEPTH - 1));
        debug_state.logged--;
    }
    struct gexgl_debug_record *slot = &debug_state.log[debug_state.logged++];
    slot->source = source; slot->type = type; slot->id = id;
    slot->severity = severity; slot->length = (int)n;
    memcpy(slot->text, text, n);
    slot->text[n] = '\0';
}

void glDebugMessageCallback(gexgl_debug_proc callback, const void *user) {
    debug_state.callback = callback;
    debug_state.user = user;
}

void glDebugMessageControl(unsigned source, unsigned type, unsigned severity,
                           int count, const unsigned *ids, unsigned char enabled) {
    (void)source; (void)type; (void)severity; (void)ids;
    /* Latched here, not forwarded: this entry point is one the guest answers
       for itself -- it joins the caller's fragments, or handles the call
       entirely locally -- so the host never sees the count the application
       passed and could not raise the error for it. Safe to answer locally
       only because the reference agrees on which error: native Mesa raises
       GL_INVALID_VALUE for all of these, unlike the multi-bind entry points
       where it answers GL_INVALID_OPERATION against the specification. */
    if (count < 0) { GEXGL_SET_ERROR(0x0501u); return; }
    /* A control call naming no ids and GL_DONT_CARE throughout is the global
       switch, and that one is honoured. */
    if (count == 0 && source == 0x1100u && type == 0x1100u && severity == 0x1100u)
        debug_state.output_enabled = enabled ? 1 : 0;
}

void glDebugMessageInsert(unsigned source, unsigned type, unsigned id,
                          unsigned severity, int length, const char *buf) {
    gexgl_debug_emit(source, type, id, severity, length, buf);
}

unsigned glGetDebugMessageLog(unsigned count, int bufSize, unsigned *sources,
                              unsigned *types, unsigned *ids, unsigned *severities,
                              int *lengths, char *messageLog) {
    unsigned written = 0;
    int used = 0;
    while (written < count && written < debug_state.logged) {
        const struct gexgl_debug_record *r = &debug_state.log[written];
        const int need = r->length + 1;
        /* GL stops before a message that will not fit and leaves it queued. */
        if (messageLog && (bufSize < 0 || used + need > bufSize)) break;
        if (sources) sources[written] = r->source;
        if (types) types[written] = r->type;
        if (ids) ids[written] = r->id;
        if (severities) severities[written] = r->severity;
        if (lengths) lengths[written] = need;
        if (messageLog) {
            memcpy(messageLog + used, r->text, (size_t)r->length);
            messageLog[used + r->length] = '\0';
            used += need;
        }
        ++written;
    }
    if (written) {
        memmove(&debug_state.log[0], &debug_state.log[written],
                sizeof(debug_state.log[0]) * (debug_state.logged - written));
        debug_state.logged -= written;
    }
    return written;
}

void glPushDebugGroup(unsigned source, unsigned id, int length,
                      const char *message) {
    if (debug_state.depth >= GEXGL_DEBUG_GROUP_DEPTH) return;
    size_t n = message ? (length < 0 ? strlen(message) : (size_t)length) : 0;
    if (n >= GEXGL_DEBUG_MESSAGE_MAX) n = GEXGL_DEBUG_MESSAGE_MAX - 1;
    unsigned at = debug_state.depth++;
    debug_state.group[at].source = source;
    debug_state.group[at].id = id;
    if (n && message) memcpy(debug_state.group[at].text, message, n);
    debug_state.group[at].text[n] = '\0';
    /* GL_DEBUG_TYPE_PUSH_GROUP, GL_DEBUG_SEVERITY_NOTIFICATION. */
    gexgl_debug_emit(source, 0x8269u, id, 0x826Bu, (int)n,
                     debug_state.group[at].text);
}

void glPopDebugGroup(void) {
    if (!debug_state.depth) return;
    const unsigned at = --debug_state.depth;
    gexgl_debug_emit(debug_state.group[at].source, 0x826Au /* POP_GROUP */,
                     debug_state.group[at].id, 0x826Bu,
                     (int)strlen(debug_state.group[at].text),
                     debug_state.group[at].text);
}

static struct gexgl_debug_label *gexgl_debug_label_slot(unsigned identifier,
                                                        unsigned name,
                                                        const void *pointer,
                                                        int create) {
    for (size_t i = 0; i < debug_state.label_count; ++i) {
        struct gexgl_debug_label *l = &debug_state.labels[i];
        if (pointer ? l->pointer == pointer
                    : (l->identifier == identifier && l->name == name &&
                       !l->pointer))
            return l;
    }
    if (!create) return NULL;
    struct gexgl_debug_label *grown =
        realloc(debug_state.labels,
                (debug_state.label_count + 1) * sizeof(*grown));
    if (!grown) return NULL;
    debug_state.labels = grown;
    struct gexgl_debug_label *l = &grown[debug_state.label_count++];
    memset(l, 0, sizeof(*l));
    l->identifier = identifier;
    l->name = name;
    l->pointer = pointer;
    return l;
}

static void gexgl_debug_set_label(unsigned identifier, unsigned name,
                                  const void *pointer, int length,
                                  const char *label) {
    struct gexgl_debug_label *slot =
        gexgl_debug_label_slot(identifier, name, pointer, 1);
    if (!slot) return;
    if (!label) { slot->text[0] = '\0'; return; }
    size_t n = length < 0 ? strlen(label) : (size_t)length;
    if (n >= GEXGL_DEBUG_LABEL_MAX) n = GEXGL_DEBUG_LABEL_MAX - 1;
    memcpy(slot->text, label, n);
    slot->text[n] = '\0';
}

static void gexgl_debug_get_label(unsigned identifier, unsigned name,
                                  const void *pointer, int bufSize,
                                  int *length, char *label) {
    const struct gexgl_debug_label *slot =
        gexgl_debug_label_slot(identifier, name, pointer, 0);
    const char *text = slot ? slot->text : "";
    const size_t n = strlen(text);
    if (label && bufSize > 0) {
        const size_t copy = n < (size_t)bufSize - 1 ? n : (size_t)bufSize - 1;
        memcpy(label, text, copy);
        label[copy] = '\0';
        if (length) *length = (int)copy;
    } else if (length) {
        *length = (int)n;
    }
}

void glObjectLabel(unsigned identifier, unsigned name, int length,
                   const char *label) {
    gexgl_debug_set_label(identifier, name, NULL, length, label);
}

void glGetObjectLabel(unsigned identifier, unsigned name, int bufSize,
                      int *length, char *label) {
    gexgl_debug_get_label(identifier, name, NULL, bufSize, length, label);
}

void glObjectPtrLabel(const void *ptr, int length, const char *label) {
    gexgl_debug_set_label(0, 0, ptr, length, label);
}

void glGetObjectPtrLabel(const void *ptr, int bufSize, int *length,
                         char *label) {
    gexgl_debug_get_label(0, 0, ptr, bufSize, length, label);
}

/* GL_DEBUG_CALLBACK_FUNCTION 0x8244, GL_DEBUG_CALLBACK_USER_PARAM 0x8245.
   The only two pointers GL 4.6 core defines for this query. */
void glGetPointerv(unsigned pname, void **params) {
    if (!params) return;
    if (pname == 0x8244u) { *params = (void *)(uintptr_t)debug_state.callback; return; }
    if (pname == 0x8245u) { *params = (void *)(uintptr_t)debug_state.user; return; }
    *params = NULL;
}

/* The error latch, defined with glGetError further down. The bounded queries
   below need it: GL requires them to raise GL_INVALID_OPERATION when the
   caller's buffer is too small, and refusing quietly is worse than useless --
   an application checks glGetError, sees success, and reads the buffer it just
   failed to have filled. That is exactly the hazard robustness exists to
   remove, so the refusal has to be reported as well as performed.
   Declared above, with the mapping helpers that also need it. */

/* Declared because they are defined below this point, some of them in the
   generated include.
 *
 * This is the fourth time in one session that composing a call out of existing
 * ones reached a function defined later in the file. C accepts it silently --
 * an implicit declaration is assumed to be int() -- and the argument
 * conventions then differ for anything that is not an int. The compiler warns,
 * so the rule is simply to build with warnings visible and declare what a new
 * composition reaches; there is no header to include, because this library
 * deliberately uses plain C types rather than the GL typedefs. */
void glReadPixels(int x, int y, int w, int h, unsigned format, unsigned type,
                  void *out);
void glGetTexLevelParameteriv(unsigned target, int level, unsigned pname,
                              int *params);
void glGetTexImage(unsigned target, int level, unsigned format, unsigned type,
                   void *pixels);
void glGetCompressedTexImage(unsigned target, int level, void *img);

/* KHR_robustness' bounded readbacks, and ARB_get_texture_sub_image.
 *
 * The whole point of the bounded forms is that GL reports INVALID_OPERATION
 * instead of writing past the caller's buffer, so each asks the host how large
 * the answer is and refuses before any bytes move. That costs a round trip the
 * unbounded twin does not pay, which is the trade the caller asked for. */
void glReadnPixels(int x, int y, int w, int h, unsigned format, unsigned type,
                   int bufSize, void *data) {
    if (bufSize < 0) return;
    /* Computed with the same pixel-store state the transfer will use, so the
       check and the copy cannot disagree. */
    struct gexgl_pixel_layout layout;
    if (!gexgl_compute_pixel_layout(format, type, w, h, -1, &pack_store,
                                    &layout))
        return;
    if (layout.end > (size_t)bufSize) {
        GEXGL_SET_ERROR(0x0502u); /* GL_INVALID_OPERATION */
        return;
    }
    glReadPixels(x, y, w, h, format, type, data);
}

void glGetnTexImage(unsigned target, int level, unsigned format, unsigned type,
                    int bufSize, void *pixels) {
    if (bufSize < 0) return;
    int width = 0, height = 0, depth = 1;
    glGetTexLevelParameteriv(target, level, 0x1000u /* GL_TEXTURE_WIDTH */, &width);
    glGetTexLevelParameteriv(target, level, 0x1001u /* GL_TEXTURE_HEIGHT */, &height);
    glGetTexLevelParameteriv(target, level, 0x8071u /* GL_TEXTURE_DEPTH */, &depth);
    if (depth < 1) depth = 1;
    const int volume = target == 0x806Fu /* 3D */ || target == 0x8C1Au /* 2D_ARRAY */ ||
                       target == 0x9009u /* CUBE_MAP_ARRAY */;
    struct gexgl_pixel_layout layout;
    if (!gexgl_compute_pixel_layout(format, type, width, height,
                                    volume ? depth : -1, &pack_store, &layout))
        return;
    if (layout.end > (size_t)bufSize) {
        GEXGL_SET_ERROR(0x0502u);
        return;
    }
    glGetTexImage(target, level, format, type, pixels);
}

void glGetnCompressedTexImage(unsigned target, int lod, int bufSize,
                              void *pixels) {
    if (bufSize < 0) return;
    int size = 0;
    glGetTexLevelParameteriv(target, lod,
                             0x86A0u /* GL_TEXTURE_COMPRESSED_IMAGE_SIZE */,
                             &size);
    if (size < 0 || size > bufSize) {
        GEXGL_SET_ERROR(0x0502u);
        return;
    }
    glGetCompressedTexImage(target, lod, pixels);
}

/* ARB_get_texture_sub_image. A region of a level rather than the whole thing,
   which is what a readback of one tile or one array layer wants, and the only
   readback GL offers that does not force the caller to allocate the lot. */
void glGetTextureSubImage(unsigned texture, int level, int x, int y, int z,
                          int w, int h, int d, unsigned format, unsigned type,
                          int bufSize, void *pixels) {
    if (bufSize < 0) return;
    const int pbo = bound_buffer(0x88EBu) != 0; /* GL_PIXEL_PACK_BUFFER */
    uint32_t *p = begin(GEXGL_GET_TEXTURE_SUB_IMAGE, 14, 0);
    if (!p) return;
    p[0] = texture; p[1] = (uint32_t)level;
    p[2] = (uint32_t)x; p[3] = (uint32_t)y; p[4] = (uint32_t)z;
    p[5] = (uint32_t)w; p[6] = (uint32_t)h; p[7] = (uint32_t)d;
    p[8] = format; p[9] = type; p[10] = (uint32_t)bufSize;
    p[11] = (uint32_t)pbo;
    p[12] = (uint32_t)(uintptr_t)pixels;
    p[13] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    flush();
    if (pbo) return;
    uint32_t bytes = 0;
    const uint8_t *source = reply_blob(&bytes);
    /* Exact length only, as everywhere else here: a short reply means the host
       refused, and a partial copy would leave the caller unable to tell. */
    if (pixels && source && bytes && (int)bytes <= bufSize)
        memcpy(pixels, source, bytes);
}

void glGetCompressedTextureSubImage(unsigned texture, int level, int x, int y,
                                    int z, int w, int h, int d, int bufSize,
                                    void *pixels) {
    /* A negative argument belongs to GL, and only the host has a GL context
       to raise the error in. Returning here left the application with no
       error and no effect. */
    const int pbo = bound_buffer(0x88EBu) != 0;
    uint32_t *p = begin(GEXGL_GET_COMPRESSED_TEXTURE_SUB_IMAGE, 12, 0);
    if (!p) return;
    p[0] = texture; p[1] = (uint32_t)level;
    p[2] = (uint32_t)x; p[3] = (uint32_t)y; p[4] = (uint32_t)z;
    p[5] = (uint32_t)w; p[6] = (uint32_t)h; p[7] = (uint32_t)d;
    p[8] = (uint32_t)bufSize; p[9] = (uint32_t)pbo;
    p[10] = (uint32_t)(uintptr_t)pixels;
    p[11] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    flush();
    if (pbo) return;
    uint32_t bytes = 0;
    const uint8_t *source = reply_blob(&bytes);
    if (pixels && source && bytes && (int)bytes <= bufSize)
        memcpy(pixels, source, bytes);
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

/* The fixed-function vertex arrays.
 *
 * glVertexPointer and its siblings are how every pre-VBO GL program submits
 * geometry, and this library exported none of them -- which is not a missing
 * feature but a missing symbol: the program fails to resolve it and never
 * starts. They work the same way as the client-side glVertexAttribPointer
 * arrays below, and reuse the same upload: the guest copies each enabled
 * array into a scratch buffer of its own and re-points the host at offset
 * zero of it, so the draw command, its `first` and any index values keep
 * their meaning.
 *
 * One state block per array. Texture coordinates have one per unit, selected
 * by glClientActiveTexture, because a multitexturing program sets two. */
#define GEXGL_MAX_CLIENT_TEXTURE_UNITS 8
enum gexgl_ff_array {
    GEXGL_FF_VERTEX,
    GEXGL_FF_NORMAL,
    GEXGL_FF_COLOR,
    GEXGL_FF_EDGE_FLAG,
    GEXGL_FF_INDEX,
    GEXGL_FF_TEXCOORD0,
    GEXGL_FF_COUNT = GEXGL_FF_TEXCOORD0 + GEXGL_MAX_CLIENT_TEXTURE_UNITS,
};
struct gexgl_ff_array_state {
    int size;              /* components; three for normals, one for the rest */
    unsigned type;
    int stride;
    const void *pointer;
    unsigned scratch;      /* upload buffer, 0 until first needed */
    unsigned char client;  /* set with no array buffer bound */
    unsigned char enabled;
};
static struct gexgl_ff_array_state gexgl_ff_arrays[GEXGL_FF_COUNT];
static int gexgl_ff_client_count;
static unsigned gexgl_client_texture_unit;

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

/* Which state block an enum names, or -1. */
static int gexgl_ff_array_of(unsigned array) {
    switch (array) {
    case 0x8074u: return GEXGL_FF_VERTEX;         /* GL_VERTEX_ARRAY */
    case 0x8075u: return GEXGL_FF_NORMAL;         /* GL_NORMAL_ARRAY */
    case 0x8076u: return GEXGL_FF_COLOR;          /* GL_COLOR_ARRAY */
    case 0x8077u: return GEXGL_FF_INDEX;          /* GL_INDEX_ARRAY */
    case 0x8078u:                                 /* GL_TEXTURE_COORD_ARRAY */
        return GEXGL_FF_TEXCOORD0 +
               (int)(gexgl_client_texture_unit < GEXGL_MAX_CLIENT_TEXTURE_UNITS
                        ? gexgl_client_texture_unit : 0u);
    case 0x8079u: return GEXGL_FF_EDGE_FLAG;      /* GL_EDGE_FLAG_ARRAY */
    default: return -1;
    }
}

static void gexgl_record_ff_array(int which, int size, unsigned type,
                                  int stride, const void *pointer) {
    if (which < 0 || which >= GEXGL_FF_COUNT) return;
    struct gexgl_ff_array_state *a = &gexgl_ff_arrays[which];
    const unsigned char was_client = a->client;
    a->size = size;
    a->type = type;
    a->stride = stride;
    a->pointer = pointer;
    /* Which buffer is bound *now* decides, for good -- as for the generic
       arrays: binding one afterwards does not retroactively capture it. */
    a->client = bound_buffer(0x8892u) == 0 && pointer != NULL;
    if (a->client != was_client)
        gexgl_ff_client_count += a->client ? 1 : -1;
}

void glClientActiveTexture(unsigned unit) {
    /* GL_TEXTURE0 is 0x84C0. */
    gexgl_client_texture_unit = unit >= 0x84C0u ? unit - 0x84C0u : unit;
    CMD1(GEXGL_CLIENT_ACTIVE_TEXTURE, unit);
}

void glEnableClientState(unsigned array) {
    const int which = gexgl_ff_array_of(array);
    if (which >= 0) gexgl_ff_arrays[which].enabled = 1;
    CMD1(GEXGL_ENABLE_CLIENT_STATE, array);
}
void glDisableClientState(unsigned array) {
    const int which = gexgl_ff_array_of(array);
    if (which >= 0) gexgl_ff_arrays[which].enabled = 0;
    CMD1(GEXGL_DISABLE_CLIENT_STATE, array);
}

/* The array shape, with an offset of zero: whatever buffer is bound on the
   host when this arrives is the one it captures. */
static void gexgl_send_ff_pointer(int which) {
    struct gexgl_ff_array_state *a = &gexgl_ff_arrays[which];
    if (which >= GEXGL_FF_TEXCOORD0) {
        uint32_t *p = begin(GEXGL_TEXCOORD_POINTER, 5, 0);
        if (p) { p[0] = (uint32_t)(which - GEXGL_FF_TEXCOORD0);
                 p[1] = (uint32_t)a->size; p[2] = a->type;
                 p[3] = (uint32_t)a->stride; p[4] = 0; }
        return;
    }
    static const uint32_t opcode[] = {
        GEXGL_VERTEX_POINTER, GEXGL_NORMAL_POINTER, GEXGL_COLOR_POINTER,
        GEXGL_EDGE_FLAG_POINTER, GEXGL_INDEX_POINTER,
    };
    uint32_t *p = begin(opcode[which], 4, 0);
    if (p) { p[0] = (uint32_t)a->size; p[1] = a->type;
             p[2] = (uint32_t)a->stride; p[3] = 0; }
}

void glVertexPointer(int size, unsigned type, int stride, const void *pointer) {
    gexgl_record_ff_array(GEXGL_FF_VERTEX, size, type, stride, pointer);
    if (!gexgl_ff_arrays[GEXGL_FF_VERTEX].client)
        gexgl_send_ff_pointer(GEXGL_FF_VERTEX);
}
void glNormalPointer(unsigned type, int stride, const void *pointer) {
    gexgl_record_ff_array(GEXGL_FF_NORMAL, 3, type, stride, pointer);
    if (!gexgl_ff_arrays[GEXGL_FF_NORMAL].client)
        gexgl_send_ff_pointer(GEXGL_FF_NORMAL);
}
void glColorPointer(int size, unsigned type, int stride, const void *pointer) {
    gexgl_record_ff_array(GEXGL_FF_COLOR, size, type, stride, pointer);
    if (!gexgl_ff_arrays[GEXGL_FF_COLOR].client)
        gexgl_send_ff_pointer(GEXGL_FF_COLOR);
}
void glTexCoordPointer(int size, unsigned type, int stride,
                       const void *pointer) {
    const int which = GEXGL_FF_TEXCOORD0 +
        (int)(gexgl_client_texture_unit < GEXGL_MAX_CLIENT_TEXTURE_UNITS
                 ? gexgl_client_texture_unit : 0u);
    gexgl_record_ff_array(which, size, type, stride, pointer);
    if (!gexgl_ff_arrays[which].client)
        gexgl_send_ff_pointer(which);
}
void glEdgeFlagPointer(int stride, const void *pointer) {
    /* GL_BOOL is not a vertex type; edge flags are unsigned bytes. */
    gexgl_record_ff_array(GEXGL_FF_EDGE_FLAG, 1, 0x1401u, stride, pointer);
    if (!gexgl_ff_arrays[GEXGL_FF_EDGE_FLAG].client)
        gexgl_send_ff_pointer(GEXGL_FF_EDGE_FLAG);
}
void glIndexPointer(unsigned type, int stride, const void *pointer) {
    gexgl_record_ff_array(GEXGL_FF_INDEX, 1, type, stride, pointer);
    if (!gexgl_ff_arrays[GEXGL_FF_INDEX].client)
        gexgl_send_ff_pointer(GEXGL_FF_INDEX);
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
/* Persistent coherent mappings, re-sent before anything on the host could
   read them.
 *
 * A mapping returned by this library is guest memory, and the bytes reach the
 * host at glUnmapBuffer or glFlushMappedBufferRange. ARB_buffer_storage's
 * point is that neither happens: GL_MAP_PERSISTENT_BIT keeps the mapping
 * across draws and GL_MAP_COHERENT_BIT promises the writes are visible with no
 * call at all. So nothing was ever sent, and a draw read whatever the buffer
 * held at creation -- zeros. gl44-buffer-storage-probe's mapped-draw and
 * second-draw cases passed natively and drew nothing in the browser, and
 * Minecraft 26.2 allocates its uniform and vertex rings exactly this way.
 *
 * There is no way to observe a guest write: wasm has no mprotect and no
 * signals, so the write barrier every real implementation of this uses is not
 * available. The mapping is therefore compared against what was last sent and
 * only the changed span goes over. That is one pass over the mapped range per
 * draw -- the cost is real for a megabyte-sized ring, and GEXGL_NO_COHERENT=1
 * turns it off for anyone measuring it -- but a ring is written a little at a
 * time, so what is *transported* stays proportional to what changed.
 *
 * Per draw, not per batch: an application writes region N, draws, writes
 * region N+1, draws again, and both draws can sit in one unflushed batch.
 * Sending the mapping once at flush time would give the first draw the second
 * region's bytes, which is what second-draw catches. */
static void gexgl_flush_mapped_writes(unsigned required) {
    static int disabled = -1;
    if (disabled < 0) disabled = getenv("GEXGL_NO_COHERENT") ? 1 : 0;
    if (disabled) return;
    for (struct buffer_object_state *object = buffer_objects; object;
         object = object->next) {
        /* `required` is the access bits a mapping must have to be worth
           sending. The draw path asks for writable-persistent-coherent
           (0xc2), because a coherent mapping promises visibility with no call
           at all and every draw could read it. A barrier asks for merely
           writable-persistent (0x42): the application owes an explicit flush
           for that one, but MemoryBarrier is a synchronisation point where a
           real driver needs no transfer -- the mapping *is* the buffer -- so
           an application that leans on it is right everywhere but here. */
        if (!object->map || !object->map_length ||
            (object->map_access & required) != required)
            continue;
        if (!object->map_sent) {
            /* Only reachable when the allocation at map time failed, since
               every mapping that gets here was given a shadow there. Nothing
               is known to have been sent, so send everything: losing the
               application's writes is the worse of the two errors. */
            object->map_sent = malloc(object->map_length);
            if (!object->map_sent) continue;
            memset(object->map_sent, ~*(const unsigned char *)object->map,
                   object->map_length);
        }
        const unsigned char *now = object->map;
        unsigned char *sent = object->map_sent;
        size_t first = 0;
        while (first < object->map_length && now[first] == sent[first]) ++first;
        if (first == object->map_length) continue;
        size_t last = object->map_length;
        while (last > first && now[last - 1] == sent[last - 1]) --last;
        const size_t length = last - first;
        /* glBufferSubData names a target, so the buffer has to be *a* bound
           one. Whichever target it is already bound to will do; when it is
           bound to none, it is bound to GL_COPY_WRITE_BUFFER for the upload
           and the previous binding put back. That target exists for exactly
           this and disturbs nothing else.
           This used to `continue` instead, which loses an application's
           writes without a word: a pool of persistent mappings has only the
           one it drew from last still bound, so every other buffer in it went
           unsent. It is the same shape as the driver's own persistent[8]. */
        unsigned target = 0;
        for (size_t i = 0; i < sizeof(buffer_targets) / sizeof(buffer_targets[0]);
             ++i)
            if (buffer_targets[i].target && buffer_targets[i].name == object->name)
                { target = buffer_targets[i].target; break; }
        const unsigned copy_write = 0x8F37u; /* GL_COPY_WRITE_BUFFER */
        unsigned restore = 0;
        const int borrowed = !target;
        if (borrowed) {
            target = copy_write;
            restore = bound_buffer(copy_write);
            glBindBuffer(copy_write, object->name);
        }
        buffer_sub_data_raw(target, (long)(object->map_offset + first), (long)length,
                        now + first);
        if (borrowed)
            glBindBuffer(copy_write, restore);
        /* Only now. Recording the span as sent before the send could be
           skipped left the mapping claiming bytes the host never saw, and the
           next diff then found nothing to send -- so a buffer that missed its
           one chance never recovered. */
        memcpy(sent + first, now + first, length);
    }
}

/* And the other direction. A persistent mapping is two-way: the application
 * writes through it and the GPU writes through it, and only the first half
 * existed here -- the shadow was filled once, at map time, and never again.
 * Anything reading GPU output through a persistent mapping (an asynchronous
 * PBO read-back, a compute result, a query written into a buffer) saw the
 * contents from whenever it mapped.
 *
 * GL says when: MemoryBarrier with CLIENT_MAPPED_BUFFER_BARRIER_BIT is the
 * point at which those writes become visible to the client, and glFinish
 * carries it too. Both are explicit and rare, so a round trip each is the
 * right price -- this is not on any draw path. */
static void gexgl_refresh_persistent_reads(void) {
    /* Guest writes first, or fetching would read over them and lose them.
       Every writable persistent mapping, not just the coherent ones: a
       non-coherent mapping written through and then handed to the GPU across
       a MemoryBarrier reaches the host nowhere else, and the barrier is what
       KHR-GL44.buffer_storage.map_persistent_draw uses to publish the value
       it seeds its atomic counter with. Its reset never arrived, so the
       counter kept climbing across draws and every count after the first was
       wrong -- which looked like the emulated geometry stage over-invoking
       until the counts were swept and turned out to be cumulative. */
    gexgl_flush_mapped_writes(0x42u);
    for (struct buffer_object_state *object = buffer_objects; object;
         object = object->next) {
        /* Readable and persistent: 0x1 | 0x40. A non-persistent read mapping
           cannot legally see a GPU write at all -- the application has to
           unmap first, and that path already refetches. */
        if (!object->map || !object->map_length ||
            (object->map_access & 0x41u) != 0x41u)
            continue;
        glGetNamedBufferSubData(object->name, (long)object->map_offset,
                                (long)object->map_length, object->map);
        /* The diff above must not now read these as the application's own
           writes and send them straight back. */
        if (object->map_sent)
            memcpy(object->map_sent, object->map, object->map_length);
    }
}

/* The draw path's flush: coherent mappings only, so its per-draw cost is
   unchanged by the barrier case above. */
static void gexgl_flush_coherent_maps(void) {
    gexgl_flush_mapped_writes(0xc2u);
}

void glMemoryBarrier(unsigned barriers);

static void gexgl_client_mapped_barrier(unsigned barriers) {
    const unsigned client_mapped = 0x00004000u;
    if (barriers & client_mapped) gexgl_refresh_persistent_reads();
}

static void gexgl_upload_client_arrays(size_t vertices, int instances) {
    gexgl_flush_coherent_maps();
    if (vertex_attrib_client_count <= 0 && gexgl_ff_client_count <= 0) return;
    const unsigned previous = bound_buffer(0x8892u); /* GL_ARRAY_BUFFER */
    /* The fixed-function arrays first, by the same rules: a copy from element
       zero, so the draw needs no adjustment. They have no divisor. */
    for (int which = 0; which < GEXGL_FF_COUNT; ++which) {
        struct gexgl_ff_array_state *a = &gexgl_ff_arrays[which];
        if (!a->enabled || !a->client || !a->pointer || !vertices) continue;
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
        glBufferData(0x8892u, (long)bytes, a->pointer, 0x88E8u);
        gexgl_send_ff_pointer(which);
    }
    if (vertex_attrib_client_count <= 0) {
        glBindBuffer(0x8892u, previous);
        return;
    }
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

/* A note, because the fix that suggests itself here is wrong.
 *
 * The cached divisors above size a client array's upload, and only
 * glVertexAttribDivisor maintains them. The separate-format API can change the
 * same state -- GL defines glVertexAttribDivisor(i, d) as
 * glVertexAttribBinding(i, i) plus glVertexBindingDivisor(i, d) -- and this
 * side tracks no binding state, so after glVertexBindingDivisor the cache can
 * be stale.
 *
 * Clearing it on those calls looks like the safe answer and is not. `needed`
 * below is ceil(instances / divisor) for an instanced attribute, and an
 * application that instances an attribute provides only that many elements --
 * so uploading the full vertex count instead reads past the array it was
 * given. Both directions are out of bounds: too large a divisor under-uploads
 * and the draw reads past the upload, too small a one over-reads the
 * application's own memory, and the second is the guest's address space.
 *
 * That hazard already exists for an attribute instanced only through
 * glVertexBindingDivisor, where the cache reads zero today. Fixing it properly
 * means tracking the attribute-to-binding map and the per-binding divisors on
 * this side, so the upload is sized from what GL actually says. Not guessed
 * at, and not "conservatively" cleared, because there is no conservative
 * direction. */
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

/* The named texture uploads. GL 4.5 core, and the last thing between an engine
   on the DSA path and a textured frame: the object model, the storage and the
   buffer uploads all landed already, and without these a texture could be
   created and sized by name and then never filled.

   Hand-written beside their bound twins for the same reason those are: the
   byte count comes from gexgl_compute_pixel_layout against the guest's own
   GL_UNPACK_* state, and both ends must compute it identically -- that shared
   calculation is a memory-safety requirement, not an optimisation. A pixel
   unpack buffer is passed as an offset instead of a blob, exactly as above.

   The only difference from the bound forms is the first word: a texture name
   here, a target there. */
void glTextureSubImage1D(unsigned texture, int level, int x, int w,
                         unsigned format, unsigned type, const void *pixels) {
    const int pbo = bound_buffer(0x88ECu) != 0;
    const size_t len = pixels && !pbo
        ? pixel_transfer_bytes(format, type, w, 1, -1, &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEXTURE_SUB_IMAGE_1D, 9, 4 + len);
    if (!p) return;
    p[0] = texture; p[1] = (uint32_t)level; p[2] = (uint32_t)x;
    p[3] = (uint32_t)w; p[4] = format; p[5] = type;
    p[6] = (uint32_t)pbo;
    /* Two words, like the 2D and 3D forms: with a pixel unpack buffer bound
       this is an offset, not a pointer, and truncating it to 32 bits would
       silently read the wrong place in a large buffer. */
    p[7] = (uint32_t)(uintptr_t)pixels;
    p[8] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(p + 9, pbo ? NULL : pixels, len);
}

void glTextureSubImage2D(unsigned texture, int level, int x, int y, int w, int h,
                         unsigned format, unsigned type, const void *pixels) {
    const int pbo = bound_buffer(0x88ECu) != 0;
    const size_t len = pixels && !pbo
        ? pixel_transfer_bytes(format, type, w, h, -1, &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEXTURE_SUB_IMAGE_2D, 11, 4 + len);
    if (!p) return;
    p[0] = texture; p[1] = (uint32_t)level; p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)w; p[5] = (uint32_t)h; p[6] = format; p[7] = type;
    p[8] = (uint32_t)pbo;
    p[9] = (uint32_t)(uintptr_t)pixels;
    p[10] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(p + 11, pbo ? NULL : pixels, len);
}

void glTextureSubImage3D(unsigned texture, int level, int x, int y, int z,
                         int w, int h, int d, unsigned format, unsigned type,
                         const void *pixels) {
    const int pbo = bound_buffer(0x88ECu) != 0;
    const size_t len = pixels && !pbo
        ? pixel_transfer_bytes(format, type, w, h, d, &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_TEXTURE_SUB_IMAGE_3D, 13, 4 + len);
    if (!p) return;
    p[0] = texture; p[1] = (uint32_t)level; p[2] = (uint32_t)x; p[3] = (uint32_t)y;
    p[4] = (uint32_t)z; p[5] = (uint32_t)w; p[6] = (uint32_t)h; p[7] = (uint32_t)d;
    p[8] = format; p[9] = type;
    p[10] = (uint32_t)pbo;
    p[11] = (uint32_t)(uintptr_t)pixels;
    p[12] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(p + 13, pbo ? NULL : pixels, len);
}

/* Defined further down, with the fixed-function set they belong to. */
static void put_float(uint32_t *slot, float value);
static size_t light_parameter_floats(unsigned pname);
static size_t material_parameter_floats(unsigned pname);

/* The pixel-rectangle family.
 *
 * glDrawPixels carries an image and is sized by the same pixel-store
 * calculation the texture uploads above use -- keeping that identical at both
 * ends is a memory-safety requirement, not an optimisation, which is why it
 * lives in gexgl-pixel.h and is shared.
 *
 * glBitmap is the exception: its GL_BITMAP packing is one bit per pixel, which
 * gexgl_compute_pixel_layout deliberately refuses, so its rows are measured
 * here. A row is ceil(width / 8) bytes rounded up to the unpack alignment,
 * and there are `height` of them. */
void glDrawPixels(int w, int h, unsigned format, unsigned type,
                  const void *pixels) {
    const int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */
    const size_t len = pixels && !pbo
        ? pixel_transfer_bytes(format, type, w, h, -1, &unpack_store, NULL) : 0;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_DRAW_PIXELS, 5, 4 + len);
    if (!p) return;
    p[0] = (uint32_t)w; p[1] = (uint32_t)h;
    p[2] = format; p[3] = type; p[4] = (uint32_t)pbo;
    put_blob(p + 5, pbo ? NULL : pixels, len);
}

void glCopyPixels(int x, int y, int w, int h, unsigned type) {
    uint32_t *p = begin(GEXGL_COPY_PIXELS, 5, 0);
    if (!p) return;
    p[0] = (uint32_t)x; p[1] = (uint32_t)y;
    p[2] = (uint32_t)w; p[3] = (uint32_t)h; p[4] = type;
}

void glBitmap(int w, int h, float xorig, float yorig, float xmove, float ymove,
              const unsigned char *bitmap) {
    const int pbo = bound_buffer(0x88ECu) != 0;
    size_t len = 0;
    if (bitmap && !pbo && w > 0 && h > 0) {
        const size_t alignment = unpack_store.alignment ? unpack_store.alignment : 1;
        const size_t row_pixels = unpack_store.row_length ? unpack_store.row_length
                                                          : (size_t)w;
        size_t row = (row_pixels + 7u) / 8u;
        row = ((row + alignment - 1u) / alignment) * alignment;
        if (row && (size_t)h > SIZE_MAX / row) return;
        len = row * (size_t)h;
    }
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_BITMAP, 7, 4 + len);
    if (!p) return;
    p[0] = (uint32_t)w; p[1] = (uint32_t)h;
    put_float(&p[2], xorig); put_float(&p[3], yorig);
    put_float(&p[4], xmove); put_float(&p[5], ymove);
    p[6] = (uint32_t)pbo;
    put_blob(p + 7, pbo ? NULL : bitmap, len);
}

void glPixelTransferf(unsigned pname, float value) {
    uint32_t *p = begin(GEXGL_PIXEL_TRANSFERF, 2, 0);
    if (!p) return;
    p[0] = pname; put_float(&p[1], value);
}
void glPixelTransferi(unsigned pname, int value) {
    uint32_t *p = begin(GEXGL_PIXEL_TRANSFERI, 2, 0);
    if (!p) return;
    p[0] = pname; p[1] = (uint32_t)value;
}

/* Colour-index mode state. The index itself is a float on the wire, which is
   what every entry point below reduces to. */
void glClearIndex(float index) {
    uint32_t *p = begin(GEXGL_CLEAR_INDEX, 1, 0);
    if (!p) return;
    put_float(&p[0], index);
}
void glIndexf(float index) {
    uint32_t *p = begin(GEXGL_INDEXF, 1, 0);
    if (!p) return;
    put_float(&p[0], index);
}
void glIndexi(int index) { glIndexf((float)index); }
void glIndexub(unsigned char index) { glIndexf((float)index); }
void glIndexd(double index) { glIndexf((float)index); }
void glIndexs(short index) { glIndexf((float)index); }

void glEdgeFlag(unsigned char flag) { CMD1(GEXGL_EDGE_FLAG, flag ? 1u : 0u); }
void glEdgeFlagv(const unsigned char *flag) { if (flag) glEdgeFlag(*flag); }

/* The integer forms of the light and material parameters. GL scales them the
   same way as the float forms; sending them as integers keeps that decision
   on the host, where the real entry point makes it. */
static void put_enum_iv(uint32_t opcode, unsigned target, unsigned pname,
                        const int *values, size_t count) {
    if (!values || !count) return;
    uint32_t *p = begin(opcode, 3 + count, 0);
    if (!p) return;
    p[0] = target; p[1] = pname; p[2] = (uint32_t)count;
    for (size_t i = 0; i < count; ++i) p[3 + i] = (uint32_t)values[i];
}
void glMaterialiv(unsigned face, unsigned pname, const int *values) {
    put_enum_iv(GEXGL_MATERIALIV, face, pname, values,
                material_parameter_floats(pname));
}
void glLightiv(unsigned light, unsigned pname, const int *values) {
    put_enum_iv(GEXGL_LIGHTIV, light, pname, values,
                light_parameter_floats(pname));
}

/* Display list names. glCallLists' array is `n` values of `type`, which is
   the one place a display list index is not a plain uint32. */
void glListBase(unsigned base) { CMD1(GEXGL_LIST_BASE, base); }
void glCallLists(int n, unsigned type, const void *lists) {
    if (n <= 0 || !lists) return;
    size_t element;
    switch (type) {
    case 0x1400: case 0x1401: element = 1; break;  /* GL_[UNSIGNED_]BYTE */
    case 0x1402: case 0x1403: element = 2; break;  /* GL_[UNSIGNED_]SHORT */
    case 0x1404: case 0x1405: element = 4; break;  /* GL_[UNSIGNED_]INT */
    case 0x1406: element = 4; break;               /* GL_FLOAT */
    case 0x1407: element = 3; break;               /* GL_2_BYTES */
    case 0x1408: element = 3; break;               /* GL_3_BYTES */
    case 0x1409: element = 4; break;               /* GL_4_BYTES */
    default: return;
    }
    if ((size_t)n > SIZE_MAX / element) return;
    const size_t len = (size_t)n * element;
    if (len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_CALL_LISTS, 2, 4 + len);
    if (!p) return;
    p[0] = (uint32_t)n; p[1] = type;
    put_blob(p + 2, lists, len);
}

/* Texture-coordinate generation.
 *
 * How a fixed-function program does sphere mapping and reflections, and
 * `texcyl` needed nothing else to start. The plane parameters are the only
 * vector form -- GL_OBJECT_PLANE and GL_EYE_PLANE take four floats; the mode
 * is a single enum. */
static size_t tex_gen_floats(unsigned pname) {
    switch (pname) {
    case 0x2501: case 0x2502: return 4; /* GL_OBJECT_PLANE, GL_EYE_PLANE */
    default: return 1;                  /* GL_TEXTURE_GEN_MODE */
    }
}
void glTexGeni(unsigned coord, unsigned pname, int param) {
    uint32_t *p = begin(GEXGL_TEX_GENI, 3, 0);
    if (!p) return;
    p[0] = coord; p[1] = pname; p[2] = (uint32_t)param;
}
void glTexGenfv(unsigned coord, unsigned pname, const float *params) {
    if (!params) return;
    const size_t count = tex_gen_floats(pname);
    uint32_t *p = begin(GEXGL_TEX_GENFV, 3 + count, 0);
    if (!p) return;
    p[0] = coord; p[1] = pname; p[2] = (uint32_t)count;
    for (size_t i = 0; i < count; ++i) put_float(&p[3 + i], params[i]);
}
void glTexGenf(unsigned coord, unsigned pname, float param) {
    glTexGenfv(coord, pname, &param);
}
void glTexGend(unsigned coord, unsigned pname, double param) {
    const float f = (float)param;
    glTexGenfv(coord, pname, &f);
}
void glTexGeniv(unsigned coord, unsigned pname, const int *params) {
    if (!params) return;
    glTexGeni(coord, pname, params[0]);
}
void glTexGendv(unsigned coord, unsigned pname, const double *params) {
    if (!params) return;
    float f[4];
    const size_t count = tex_gen_floats(pname);
    for (size_t i = 0; i < count; ++i) f[i] = (float)params[i];
    glTexGenfv(coord, pname, f);
}

/* glWindowPos: the raster position, in window coordinates, bypassing the
   transform. `wincopy` and `glxsnoop` place their pixel rectangles with it. */
void glWindowPos3f(float x, float y, float z) {
    uint32_t *p = begin(GEXGL_WINDOW_POS3F, 3, 0);
    if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y); put_float(&p[2], z);
}
void glWindowPos2f(float x, float y) { glWindowPos3f(x, y, 0.0f); }
void glWindowPos2i(int x, int y) { glWindowPos3f((float)x, (float)y, 0.0f); }
void glWindowPos2d(double x, double y) { glWindowPos3f((float)x, (float)y, 0.0f); }
void glWindowPos2s(short x, short y) { glWindowPos3f(x, y, 0.0f); }
void glWindowPos3i(int x, int y, int z) { glWindowPos3f((float)x, (float)y, (float)z); }
void glWindowPos2fv(const float *v) { if (v) glWindowPos3f(v[0], v[1], 0.0f); }
void glWindowPos2iv(const int *v) { if (v) glWindowPos2i(v[0], v[1]); }
void glWindowPos3fv(const float *v) { if (v) glWindowPos3f(v[0], v[1], v[2]); }
/* The ARB spellings -- glWindowPos2iARB and the rest -- are what the
   applications actually link against, and gl-aliases-generated.inc emits them
   from the registry for every core name that exists here. Defining them by
   hand collides with that. */

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
/* Two DSA calls that need an answer back, not just bytes out.
 *
 * Both flush and wait, exactly as their bound twins do: the ring is
 * one-directional and the reply is read after the batch is submitted. That
 * cost is why GL has so few of these, and why an engine calls
 * glCheckNamedFramebufferStatus once at setup rather than per frame. */
unsigned glCheckNamedFramebufferStatus(unsigned framebuffer, unsigned target) {
    uint32_t *p = begin(GEXGL_CHECK_NAMED_FRAMEBUFFER_STATUS, 2, 0);
    if (!p) return 0;
    p[0] = framebuffer;
    p[1] = target;
    flush();
    const unsigned status = reply_u32();
    if (status != 0x8CD5u)
        fprintf(stderr,
                "gex-gl: framebuffer %u status 0x%x for target 0x%x\n",
                framebuffer, status, target);
    return status;
}

void glGetNamedBufferSubData(unsigned buffer, long offset, long size,
                             void *data) {
    /* A negative argument belongs to GL, and only the host has a GL context
       to raise the error in. Returning here left the application with no
       error and no effect. */

    uint32_t *p = begin(GEXGL_GET_NAMED_BUFFER_SUB_DATA, 5, 0);
    if (!p) return;
    const uint64_t off = (uint64_t)offset, bytes = (uint64_t)size;
    p[0] = buffer;
    p[1] = (uint32_t)off; p[2] = (uint32_t)(off >> 32);
    p[3] = (uint32_t)bytes; p[4] = (uint32_t)(bytes >> 32);
    flush();
    uint32_t reply_len = 0;
    const uint8_t *reply = reply_blob(&reply_len);
    /* Only on an exact-length reply: a short one means the host refused or
       clamped, and copying it would leave the caller's buffer half updated
       with no way to tell. */
    if (data && size > 0 && reply && reply_len == (uint64_t)size)
        memcpy(data, reply, reply_len);
}

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

/* Every indexed draw except plain glDrawElements, which keeps its own opcode.
 *
 * The whole family used to be generated from the registry as an
 * *element-buffer offset*, on the reasoning that "in an OpenGL 4.4 core
 * context these are never client-memory pointers". True, and the guest hands
 * out compatibility contexts -- where `indices` is a client pointer whenever
 * no element array buffer is bound. Sent as an offset, it had the host read
 * its own address space; and nothing uploaded the client vertex arrays
 * either, so the draw read zeroes and covered no pixels. `glDrawRangeElements`
 * is the GL 1.2 form most old code actually calls.
 *
 * One command covers all of them because what they share is the part that
 * matters. Base vertex, instancing and base instance are fields the host
 * dispatches on; glDrawRangeElements' start/end are dropped, which is legal
 * -- they are a promise about the indices, not a change to what is drawn.
 */
static void gexgl_draw_elements_full(unsigned mode, int count, unsigned type,
                                     const void *indices, int basevertex,
                                     int instances, unsigned baseinstance) {
    const int client = bound_buffer(0x8893u) == 0; /* GL_ELEMENT_ARRAY_BUFFER */
    const size_t element = index_element_bytes(type);
    if ((vertex_attrib_client_count > 0 || gexgl_ff_client_count > 0) &&
        client) {
        size_t extent = gexgl_client_index_extent(indices, count, type);
        /* The host reads vertex `index + basevertex`, so the copy has to
           reach that far. A negative base only ever reads lower. */
        if (extent && basevertex > 0) extent += (size_t)basevertex;
        gexgl_upload_client_arrays(extent, instances > 0 ? instances : 1);
    }
    size_t blob_len = 0;
    if (client && indices && count > 0 && element &&
        (size_t)count <= SIZE_MAX / element)
        blob_len = (size_t)count * element;
    if (blob_len > UINT32_MAX - 4u) return;
    uint32_t *p = begin(GEXGL_DRAW_ELEMENTS_FULL, 8,
                        blob_len ? 4 + blob_len : 0);
    if (!p) return;
    p[0] = mode;
    p[1] = (uint32_t)count;
    p[2] = type;
    p[3] = (uint32_t)basevertex;
    p[4] = (uint32_t)instances;
    p[5] = baseinstance;
    p[6] = (uint32_t)(uintptr_t)indices;
    p[7] = (uint32_t)(blob_len != 0);
    if (blob_len)
        put_blob(p + 8, indices, blob_len);
}

void glDrawRangeElements(unsigned mode, unsigned start, unsigned end,
                         int count, unsigned type, const void *indices) {
    (void)start; (void)end;
    gexgl_draw_elements_full(mode, count, type, indices, 0, 0, 0);
}

void glDrawRangeElementsBaseVertex(unsigned mode, unsigned start, unsigned end,
                                   int count, unsigned type,
                                   const void *indices, int basevertex) {
    (void)start; (void)end;
    gexgl_draw_elements_full(mode, count, type, indices, basevertex, 0, 0);
}

void glDrawElementsBaseVertex(unsigned mode, int count, unsigned type,
                              const void *indices, int basevertex) {
    gexgl_draw_elements_full(mode, count, type, indices, basevertex, 0, 0);
}

void glDrawElementsInstancedBaseVertex(unsigned mode, int count, unsigned type,
                                       const void *indices, int instancecount,
                                       int basevertex) {
    gexgl_draw_elements_full(mode, count, type, indices, basevertex,
                             instancecount, 0);
}

void glDrawElementsInstancedBaseInstance(unsigned mode, int count,
                                         unsigned type, const void *indices,
                                         int instancecount,
                                         unsigned baseinstance) {
    gexgl_draw_elements_full(mode, count, type, indices, 0, instancecount,
                             baseinstance);
}

void glDrawElementsInstancedBaseVertexBaseInstance(
    unsigned mode, int count, unsigned type, const void *indices,
    int instancecount, int basevertex, unsigned baseinstance) {
    gexgl_draw_elements_full(mode, count, type, indices, basevertex,
                             instancecount, baseinstance);
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
       out of reach and the draw is left to the host as before.
       The fixed-function arrays count too. Testing only the generic ones left
       glVertexPointer/glColorPointer draws with no upload at all, so the host
       drew from whatever a previous glDrawArrays had left in the scratch
       buffers -- which for an index that reached past that draw's vertex
       count is out of bounds, and an out-of-bounds vertex fetch reads zero.
       tests/gl44-probe.sh point caught it as two points that drew nothing:
       the same draw at size 1, which does not go near the wide-point
       expansion, drew nothing either, which is what said the expansion was
       innocent. */
    if ((vertex_attrib_client_count > 0 || gexgl_ff_client_count > 0) &&
        client)
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
    /* Through the shared path, which is what gives it the client-array upload
       and the indices on the wire. Its own opcode sent neither. */
    gexgl_draw_elements_full(mode, count, type, offset, 0, instances, 0);
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
/* The named framebuffer clears. GL 4.5 core; the same three values with the
   framebuffer given instead of bound. The component count follows GL: GL_COLOR
   (0x1800) takes four, GL_DEPTH and GL_STENCIL one -- reading four from a
   one-element array is how a caller's stack gets read past its end, so the
   count is applied here rather than always copying four. */
void glClearNamedFramebufferfv(unsigned framebuffer, unsigned buffer,
                               int drawbuffer, const float *value) {
    uint32_t *p = begin(GEXGL_CLEAR_NAMED_FRAMEBUFFER_FV, 7, 0);
    if (!p) return;
    p[0] = framebuffer; p[1] = buffer; p[2] = (uint32_t)drawbuffer;
    const int count = buffer == 0x1800u ? 4 : 1;
    for (int i = 0; i < 4; i++) {
        const float v = i < count && value ? value[i] : 0.0f;
        memcpy(&p[3 + i], &v, 4);
    }
}

void glClearNamedFramebufferiv(unsigned framebuffer, unsigned buffer,
                               int drawbuffer, const int *value) {
    uint32_t *p = begin(GEXGL_CLEAR_NAMED_FRAMEBUFFER_IV, 7, 0);
    if (!p) return;
    p[0] = framebuffer; p[1] = buffer; p[2] = (uint32_t)drawbuffer;
    const int count = buffer == 0x1800u ? 4 : 1;
    for (int i = 0; i < 4; i++)
        p[3 + i] = (uint32_t)(i < count && value ? value[i] : 0);
}

void glClearNamedFramebufferuiv(unsigned framebuffer, unsigned buffer,
                                int drawbuffer, const unsigned *value) {
    uint32_t *p = begin(GEXGL_CLEAR_NAMED_FRAMEBUFFER_UIV, 7, 0);
    if (!p) return;
    p[0] = framebuffer; p[1] = buffer; p[2] = (uint32_t)drawbuffer;
    const int count = buffer == 0x1800u ? 4 : 1;
    for (int i = 0; i < 4; i++)
        p[3 + i] = i < count && value ? value[i] : 0u;
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
    const unsigned pack = bound_buffer(0x88EBu); /* GL_PIXEL_PACK_BUFFER */
    int pbo = pack != 0;
    /* GL forbids packing into a buffer that is mapped, unless the mapping is
       persistent. The host is never told about the mapping, so it cannot
       raise this; the guest holds the only copy of the fact. */
    {
        const struct buffer_object_state *object = buffer_object(pack, 0);
        if (object && object->map && !(object->map_access & 0x0040u)) {
            GEXGL_SET_ERROR(0x0502u); /* GL_INVALID_OPERATION */
            return;
        }
    }
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

/* GEX_GL_LOG_ERRORS=1 names the call that latched an error. The guest raises
   GL's errors for everything it answers locally -- mapping above all -- and a
   spurious one is invisible otherwise: the application sees GL_INVALID_VALUE
   with nothing to say where it came from, and the CTS reports it against
   whichever case happened to check next. */
static void gexgl_set_error_at(unsigned error, const char *who) {
    static int on = -1;
    if (on < 0) on = getenv("GEX_GL_LOG_ERRORS") ? 1 : 0;
    if (on) fprintf(stderr, "gexgl: error 0x%04x set by %s\n", error, who);
    gexgl_set_error(error);
}

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

/* Defined below, next to the list it builds. */
static const char *gexgl_extension_string(void);

const unsigned char *glGetString(unsigned name) {
    /* GL_SHADING_LANGUAGE_VERSION is asked of the host like every other
       string. It used to be answered here with the literal "4.40 MobileGL
       WebGPU", which was three things wrong at once: it named a backend that
       is gone, it claimed a language version the context does not have -- the
       host reports 3.30 -- and an application that believes it writes
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
    "GL_MOBILEGL_webgpu",
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
    /* Defined with the other EGL state below this point; the profile mask
       query is the only thing above it that needs it. */
    extern int gexgl_egl_context_profile(void);
    if (pname == 0x9126u) { /* GL_CONTEXT_PROFILE_MASK */
        /* Whichever profile was actually asked for. This answered the core
           bit unconditionally, so a context created with
           EGL_CONTEXT_OPENGL_PROFILE_MASK set to compatibility -- and one
           that got compatibility by default, which is every application that
           names no version -- reported core while glGetString(GL_VERSION)
           from the host said "Compatibility Profile". The two contradicted
           each other, and an application deciding whether fixed function is
           available reads this one. gl44-matrix-query-probe refused to test
           anything on the strength of it, which is how it was found. */
        if (out)
            out[0] = egl_api != 0x30A2u ? 0
                   : gexgl_egl_context_profile() ? 2 /* COMPATIBILITY_BIT */
                                                 : 1; /* CORE_BIT */
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

/* How many values *one index* of an indexed query yields.
 *
 * Not gexgl_state_query_components: that answers the plain query, where
 * GL_MAX_COMPUTE_WORK_GROUP_COUNT is three values. glGetIntegeri_v of the
 * same pname is one value per index, and only GL_VIEWPORT and GL_SCISSOR_BOX
 * -- ARB_viewport_array's whole point -- are vectors per index. The host side
 * applies the same rule. */
static size_t gexgl_indexed_query_components(unsigned pname) {
    switch (pname) {
    case 0x0BA2u: /* GL_VIEWPORT */
    case 0x0C10u: /* GL_SCISSOR_BOX */
        return 4;
    default:
        return 1;
    }
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
    /* The compatibility profile's own multi-valued state. This table and the
       host's are one table in two places -- they have to agree exactly, or
       the reply is the wrong length and the guest drops it whole -- and
       neither had any of these, so they agreed on the wrong answer: one.
       glGetFloatv(GL_MODELVIEW_MATRIX, m) carried a single float and left the
       caller's other fifteen as they were, with no error. A GLX context is a
       compatibility context and fixed-function software reads its matrices
       back constantly. */
    case 0x0BA6u: /* GL_MODELVIEW_MATRIX */
    case 0x0BA7u: /* GL_PROJECTION_MATRIX */
    case 0x0BA8u: /* GL_TEXTURE_MATRIX */
    case 0x80B1u: /* GL_COLOR_MATRIX */
    /* And the transposed spellings GL 1.3 added beside them, which are the
       same sixteen values the other way up. */
    case 0x84E3u: /* GL_TRANSPOSE_MODELVIEW_MATRIX */
    case 0x84E4u: /* GL_TRANSPOSE_PROJECTION_MATRIX */
    case 0x84E5u: /* GL_TRANSPOSE_TEXTURE_MATRIX */
    case 0x84E6u: /* GL_TRANSPOSE_COLOR_MATRIX */
        return 16;
    case 0x0B00u: /* GL_CURRENT_COLOR */
    case 0x8459u: /* GL_CURRENT_SECONDARY_COLOR */
    case 0x0B03u: /* GL_CURRENT_TEXTURE_COORDS */
    case 0x0B04u: /* GL_CURRENT_RASTER_COLOR */
    case 0x0B07u: /* GL_CURRENT_RASTER_POSITION */
    case 0x0B06u: /* GL_CURRENT_RASTER_TEXTURE_COORDS */
    case 0x845Fu: /* GL_CURRENT_RASTER_SECONDARY_COLOR */
    case 0x0B66u: /* GL_FOG_COLOR */
    case 0x0B53u: /* GL_LIGHT_MODEL_AMBIENT */
    case 0x0B80u: /* GL_ACCUM_CLEAR_VALUE */
    case 0x0DD2u: /* GL_MAP2_GRID_DOMAIN */
        return 4;
    case 0x0B02u: /* GL_CURRENT_NORMAL */
    case 0x8129u: /* GL_POINT_DISTANCE_ATTENUATION */
        return 3;
    case 0x0DD0u: /* GL_MAP1_GRID_DOMAIN */
    case 0x0DD3u: /* GL_MAP2_GRID_SEGMENTS */
        return 2;
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
    default: return 1;
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
        GEXGL_SET_ERROR(0x0500u); /* GL_INVALID_ENUM */
        return NULL;
    }
    if (flags != 0) {
        GEXGL_SET_ERROR(0x0501u); /* GL_INVALID_VALUE */
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
        GEXGL_SET_ERROR(0x0501u); /* GL_INVALID_VALUE */
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
        GEXGL_SET_ERROR(0x0501u); /* GL_INVALID_VALUE */
        return 0x911Du; /* GL_WAIT_FAILED */
    }
    if (flags & ~0x00000001u) { /* only GL_SYNC_FLUSH_COMMANDS_BIT */
        GEXGL_SET_ERROR(0x0501u);
        return 0x911Du;
    }
    return 0x911Au; /* GL_ALREADY_SIGNALED */
}

void glWaitSync(void *sync, unsigned flags, uint64_t timeout) {
    if (!sync_valid((uintptr_t)sync) || flags != 0 ||
        timeout != 0xFFFFFFFFFFFFFFFFull /* GL_TIMEOUT_IGNORED */)
        GEXGL_SET_ERROR(0x0501u); /* GL_INVALID_VALUE */
}

void glGetSynciv(void *sync, unsigned pname, int buf_size, int *length, int *values) {
    if (!sync_valid((uintptr_t)sync)) {
        GEXGL_SET_ERROR(0x0501u); /* GL_INVALID_VALUE */
        return;
    }
    if (buf_size < 0) {
        GEXGL_SET_ERROR(0x0501u);
        return;
    }
    int value;
    switch (pname) {
    case 0x9112: value = 0x9116; break; /* GL_OBJECT_TYPE: GL_SYNC_FENCE */
    case 0x9113: value = 0x9117; break; /* GL_SYNC_CONDITION */
    case 0x9114: value = 0x9119; break; /* GL_SYNC_STATUS: GL_SIGNALED */
    case 0x9115: value = 0; break;      /* GL_SYNC_FLAGS */
    default:
        GEXGL_SET_ERROR(0x0500u); /* GL_INVALID_ENUM */
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

/* Per-context state, and the handles that name it.
 *
 * eglCreateContext used to return the constant (void *)3 for every call, so an
 * application with two contexts held two copies of one handle and every
 * eglMakeCurrent was a no-op. Everything this library caches -- pixel store,
 * buffer bindings, vertex attribute layout, the fixed-function arrays -- is a
 * single process-global set, and the host had one GL context behind it, so the
 * two contexts shared all of it. gl44-two-contexts-probe measures the
 * consequence: 6/6 against native Mesa and 4/6 here.
 *
 * The first context keeps the handle it always had, and its path through this
 * file is unchanged: nothing an application with one context does can reach the
 * code below. Only a second eglCreateContext allocates a slot, asks the host
 * for another GL context with CONTEXT_ADD, and starts switching.
 *
 * Objects stay shared, because the host creates the additional context sharing
 * the first's -- see CONTEXT_ADD. eglCreateContext's `share` argument is still
 * ignored, which is wrong for a program that asks for an *unshared* context;
 * it is the same wrong as before rather than a new one, and separating the
 * namespaces without implementing sharing would break the toolkits that have
 * been getting sharing by accident. */
#define GEXGL_FIRST_CONTEXT_HANDLE ((void *)3)
#define GEXGL_MAX_GUEST_CONTEXTS 8

struct gexgl_context_slot {
    void *handle;            /* what eglCreateContext returned */
    unsigned host_index;     /* the host's context, 0 for the first */
    int host_made;           /* whether CONTEXT_ADD has run for it */
    /* The caches, parked while another context is current. */
    struct gexgl_pixel_store pack, unpack;
    struct buffer_target_state buffer_targets[24];
    struct vertex_attrib_state vertex_attribs[GEXGL_MAX_VERTEX_ATTRIB_POINTERS];
    int vertex_attrib_client_count;
    struct gexgl_ff_array_state ff_arrays[GEXGL_FF_COUNT];
    int ff_client_count;
    unsigned client_texture_unit;
};
static struct gexgl_context_slot egl_context_slots[GEXGL_MAX_GUEST_CONTEXTS];
static unsigned egl_context_count;
static unsigned egl_context_current_slot;

/* Park the live caches in a slot, and take a slot's caches live. Swapping the
   globals rather than threading a context pointer through every access site is
   what keeps this change small: nothing that reads pack_store or
   vertex_attribs has to know a context switch can happen. */
static void gexgl_context_save(struct gexgl_context_slot *slot) {
    slot->pack = pack_store;
    slot->unpack = unpack_store;
    memcpy(slot->buffer_targets, buffer_targets, sizeof(buffer_targets));
    memcpy(slot->vertex_attribs, vertex_attribs, sizeof(vertex_attribs));
    slot->vertex_attrib_client_count = vertex_attrib_client_count;
    memcpy(slot->ff_arrays, gexgl_ff_arrays, sizeof(gexgl_ff_arrays));
    slot->ff_client_count = gexgl_ff_client_count;
    slot->client_texture_unit = gexgl_client_texture_unit;
}

static void gexgl_context_load(const struct gexgl_context_slot *slot) {
    pack_store = slot->pack;
    unpack_store = slot->unpack;
    memcpy(buffer_targets, slot->buffer_targets, sizeof(buffer_targets));
    memcpy(vertex_attribs, slot->vertex_attribs, sizeof(vertex_attribs));
    vertex_attrib_client_count = slot->vertex_attrib_client_count;
    memcpy(gexgl_ff_arrays, slot->ff_arrays, sizeof(gexgl_ff_arrays));
    gexgl_ff_client_count = slot->ff_client_count;
    gexgl_client_texture_unit = slot->client_texture_unit;
}

/* A fresh context starts at GL's defaults, not at whatever the last one left.
   Only the two pixel-store blocks have non-zero defaults. */
static void gexgl_context_defaults(struct gexgl_context_slot *slot) {
    void *handle = slot->handle;
    const unsigned host = slot->host_index;
    const int made = slot->host_made;
    memset(slot, 0, sizeof(*slot));
    slot->handle = handle;
    slot->host_index = host;
    slot->host_made = made;
    slot->pack = (struct gexgl_pixel_store){4, 0, 0, 0, 0, 0};
    slot->unpack = (struct gexgl_pixel_store){4, 0, 0, 0, 0, 0};
}

static void gexgl_context_switch_to(unsigned slot_index) {
    if (slot_index >= egl_context_count ||
        slot_index == egl_context_current_slot)
        return;
    gexgl_context_save(&egl_context_slots[egl_context_current_slot]);
    struct gexgl_context_slot *to = &egl_context_slots[slot_index];
    /* The host side, first time only: an additional GL context sharing the
       first's objects. A host that cannot make one replies with index zero,
       and then both slots drive the same GL context -- the old behaviour,
       which is wrong but is what the application had before. */
    if (!to->host_made) {
        to->host_made = 1;
        uint32_t *p = begin(GEXGL_CONTEXT_ADD, 0, 0);
        if (p) {
            flush();
            to->host_index = reply_u32();
        }
    }
    CMD1(GEXGL_CONTEXT_SET_CURRENT, to->host_index);
    gexgl_context_load(to);
    egl_context_current_slot = slot_index;
}
static void *egl_current_draw;
static void *egl_current_read;
static int egl_surface_width = 256;
static int egl_surface_height = 256;
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
/* Read by glGetIntegerv(GL_CONTEXT_PROFILE_MASK), which sits above this. */
int gexgl_egl_context_profile(void) { return egl_context_profile; }

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
        return;
    }
    host_context_made = 1;
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

void *eglCreateWindowSurface(void *dpy, void *config, void *win,
                             const int *attrs) {
    (void)dpy; (void)config; (void)attrs;
    int w = 0, h = 0;
    if (gexgl_x11_window_size(win, &w, &h)) {
        egl_surface_width = w;
        egl_surface_height = h;
        /* The host surface is what reaches the canvas, so the application's
           window size is the one to present at. A later resize of the window
           is not tracked -- nothing here has asked for one. */
        CMD2(GEXGL_SURFACE_RESIZE, w, h);
    }
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
        egl_surface_width = w;
        egl_surface_height = h;
        CMD2(GEXGL_SURFACE_RESIZE, w, h);
    }
    return (void *)2;
}

unsigned eglDestroySurface(void *dpy, void *surface) {
    (void)dpy; (void)surface;
    return EGL_TRUE;
}

void *eglCreateContext(void *dpy, void *config, void *share, const int *attrs) {
    (void)dpy; (void)config; (void)share;
    egl_context_major = egl_api == 0x30A2u ? 4 : 3;
    egl_context_minor = egl_api == 0x30A2u ? 4 : 0;
    int asked_version = 0, asked_profile = 0;
    if (attrs) {
        for (const int *a = attrs; *a != 0x3038; a += 2) {
            if (a[0] == 0x3098) { egl_context_major = a[1]; asked_version = 1; }
            else if (a[0] == 0x30FB) egl_context_minor = a[1];
            /* EGL_CONTEXT_OPENGL_PROFILE_MASK, which EGL 1.5 defines and this
               used to ignore: bit 0 is core, bit 1 compatibility. */
            else if (a[0] == 0x30FD) {
                egl_context_profile = (a[1] & 0x2) ? 1 : 0;
                asked_profile = 1;
            }
        }
    }
    /* An application that names no version gets a compatibility profile.
     *
       EGL's profile mask only means anything for a requested version of 3.2 or
       later; below that the profile is not selectable and desktop GL gives the
       compatibility one. EGL_CONTEXT_MAJOR_VERSION itself defaults to 1, so an
       empty attribute list is a request for GL 1.0 -- which is why native Mesa
       answers one with 4.6 Compatibility.

       This defaulted to core instead, and the difference is not academic: a
       core profile has no default vertex array object and no legacy GLSL, so
       GL2-era code that names no version had `shadow2D` rejected and every
       draw dropped before the driver saw it. An application that does name
       3.2 or later, or that names a profile mask, is left exactly as it was. */
    if (egl_api == 0x30A2u && !asked_profile &&
        (!asked_version || egl_context_major < 3 ||
         (egl_context_major == 3 && egl_context_minor < 2)))
        egl_context_profile = 1;
    /* Deliberately *not* context_create() here.
     *
     * A context becomes current at eglMakeCurrent, not at eglCreateContext,
     * and creating the host context is what makes it current. Doing it here
     * made it current against whatever the drawable was at the time -- the
     * canvas -- and GL latches the initial viewport at first make-current.
     * A program that then created an 8x8 pbuffer and drew without calling
     * glViewport had its geometry placed by a 320x240 viewport into an 8x8
     * attachment: a point landed outside it entirely and drew nothing, and a
     * full-viewport triangle covered the attachment but read back from
     * somewhere else. Both looked like defects in points and in read-back.
     *
     * eglMakeCurrent creates it instead, by which time the surface exists and
     * its size has reached the host. */
    /* The first context keeps the handle this function has always returned,
       so an application with one context sees no change at all -- and every
       program in the app sweep has one. A second gets a distinct handle,
       which it has to: two contexts that compare equal made every
       eglMakeCurrent between them a no-op, which is how their state came to
       be shared. */
    if (egl_context_count == 0) {
        egl_context_slots[0].handle = GEXGL_FIRST_CONTEXT_HANDLE;
        egl_context_slots[0].host_index = 0;
        egl_context_slots[0].host_made = 1;
        gexgl_context_defaults(&egl_context_slots[0]);
        egl_context_count = 1;
        egl_context_current_slot = 0;
        return GEXGL_FIRST_CONTEXT_HANDLE;
    }
    if (egl_context_count >= GEXGL_MAX_GUEST_CONTEXTS) {
        static int said;
        if (!said) {
            said = 1;
            fprintf(stderr, "gex-gl: more than %d GL contexts; the newest "
                            "share the last one's state\n",
                    GEXGL_MAX_GUEST_CONTEXTS);
        }
        return egl_context_slots[GEXGL_MAX_GUEST_CONTEXTS - 1].handle;
    }
    const unsigned slot = egl_context_count++;
    egl_context_slots[slot].handle =
        (void *)(uintptr_t)(0x3000u + slot);
    egl_context_slots[slot].host_index = 0;
    egl_context_slots[slot].host_made = 0;
    gexgl_context_defaults(&egl_context_slots[slot]);
    return egl_context_slots[slot].handle;
}

unsigned eglDestroyContext(void *dpy, void *ctx) { (void)dpy; (void)ctx; return EGL_TRUE; }

unsigned eglMakeCurrent(void *dpy, void *draw, void *read, void *ctx) {
    if (!egl_ready) return EGL_FALSE;
    /* A safety net for callers that reach a current context without going
       through this library's eglCreateContext. The latch inside makes it a
       no-op once the host context exists. */
    if (ctx) context_create();
    /* Switch the cached state and the host's context when the application
       moves between two of them. A single-context program never gets here
       with anything but slot zero, so its path is unchanged. */
    if (ctx) {
        for (unsigned i = 0; i < egl_context_count; ++i) {
            if (egl_context_slots[i].handle == ctx) {
                gexgl_context_switch_to(i);
                break;
            }
        }
    }
    egl_current_display = ctx ? dpy : NULL;
    egl_current_context = ctx;
    egl_current_draw = ctx ? draw : NULL;
    egl_current_read = ctx ? read : NULL;
    return EGL_TRUE;
}

/* EGL's default is 1 -- an application that never calls eglSwapInterval is
   asking to be paced, and most never call it. */
static int egl_swap_interval = 1;
static uint32_t egl_swaps_issued;

unsigned eglSwapBuffers(void *dpy, void *surface) {
    (void)dpy; (void)surface;
    /* Preserve the swap boundary in the wire stream. WebGL displays its
       default framebuffer automatically, while explicit APIs such as WebGPU
       must acquire and submit a surface image here. */
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
   have linked against, so a loader-style application works unchanged. */
void *eglGetProcAddress(const char *name) {
    if (!name) return NULL;
    void *proc = dlsym(RTLD_DEFAULT, name);
    if (proc) return proc;

    /* CTS loads libEGL with RTLD_LOCAL. On musl, RTLD_DEFAULT does not search
       that local object's symbol scope, even when the lookup originates in
       the object itself. Reopen the already-loaded object to obtain its
       handle and search it directly. libEGL and libGLESv2 are two names for
       this one marshaller, so either name may be the one the application used. */
    static void *self;
    if (!self) {
        self = dlopen("libEGL.so", RTLD_LAZY | RTLD_LOCAL);
        if (!self) self = dlopen("libEGL.so.1", RTLD_LAZY | RTLD_LOCAL);
        if (!self) self = dlopen("libGLESv2.so.2", RTLD_LAZY | RTLD_LOCAL);
    }
    return self ? dlsym(self, name) : NULL;
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
    /* GL_AMBIENT_AND_DIFFUSE is 0x1602 and takes four; GL_COLOR_INDEXES is
       0x1603 and takes three. This said three for 0x1602 -- and the comment
       beside it said which was which, so the note was right and the code
       disagreed with it. glMaterialfv(face, GL_AMBIENT_AND_DIFFUSE, rgba) is
       how fixed-function content colours a lit object, and it sent three
       floats: the host fills a zeroed array with the count it is given, so
       every such material came out with an alpha of zero. */
    case 0x1602: return 4;   /* GL_AMBIENT_AND_DIFFUSE */
    case 0x1603: return 3;   /* GL_COLOR_INDEXES */
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

/* Reading fixed-function state back.
 *
 * The setters above have been here since glxgears needed them and the getters
 * never were -- and a missing entry point is not a wrong answer: the guest's
 * libGL does not export the symbol, so a program that reads this state fails
 * to link and never starts. Query, change, put back is the usual reason to
 * read it.
 *
 * Each sends the value count this side computed and the host fills that many.
 * One table, deliberately: gexgl_state_query_components has to be kept
 * identical on both sides and silently agreed on a wrong answer for every
 * pname neither copy listed. */
static size_t tex_env_floats(unsigned pname) {
    return pname == 0x2201u /* GL_TEXTURE_ENV_COLOR */ ? 4u : 1u;
}

static void get_parameter(uint32_t opcode, unsigned target, unsigned pname,
                          void *out, size_t count, size_t element) {
    if (!out || !count) return;
    uint32_t *p = begin(opcode, 3, 0);
    if (!p) return;
    p[0] = target;
    p[1] = pname;
    p[2] = (uint32_t)count;
    flush();
    uint32_t len = 0;
    const uint8_t *source = reply_blob(&len);
    if (source && len == count * element) memcpy(out, source, len);
}

void glGetLightfv(unsigned light, unsigned pname, float *params) {
    get_parameter(GEXGL_GET_LIGHTFV, light, pname, params,
                  light_parameter_floats(pname), sizeof(float));
}
void glGetLightiv(unsigned light, unsigned pname, int *params) {
    get_parameter(GEXGL_GET_LIGHTIV, light, pname, params,
                  light_parameter_floats(pname), sizeof(int));
}
void glGetMaterialfv(unsigned face, unsigned pname, float *params) {
    get_parameter(GEXGL_GET_MATERIALFV, face, pname, params,
                  material_parameter_floats(pname), sizeof(float));
}
void glGetMaterialiv(unsigned face, unsigned pname, int *params) {
    get_parameter(GEXGL_GET_MATERIALIV, face, pname, params,
                  material_parameter_floats(pname), sizeof(int));
}
void glGetTexEnvfv(unsigned target, unsigned pname, float *params) {
    get_parameter(GEXGL_GET_TEX_ENVFV, target, pname, params,
                  tex_env_floats(pname), sizeof(float));
}
void glGetTexEnviv(unsigned target, unsigned pname, int *params) {
    get_parameter(GEXGL_GET_TEX_ENVIV, target, pname, params,
                  tex_env_floats(pname), sizeof(int));
}
/* Four doubles, and no pname: the plane is the only selector. */
void glGetClipPlane(unsigned plane, double *equation) {
    get_parameter(GEXGL_GET_CLIP_PLANE, plane, 0u, equation, 4u,
                  sizeof(double));
}

/* The rest of the fixed-function set.
 *
 * A missing entry point here is not a missing feature: libGL exports a fixed
 * list of symbols, so a program calling one that is absent fails to resolve
 * it and never starts. 181 of the 341 installed GL programs needed at least
 * one symbol this library did not export, and none of that is visible in a
 * native sweep, where they link the system's own libGL.
 *
 * The vector forms are folded into the widest one -- glColor3f sends alpha 1,
 * glTexCoord2f sends r 0 and q 1 -- so one opcode serves a family. That is
 * what GL itself specifies those calls to mean.
 */
static void put_f4(uint32_t opcode, float a, float b, float c, float d) {
    uint32_t *p = begin(opcode, 4, 0);
    if (!p) return;
    put_float(&p[0], a); put_float(&p[1], b);
    put_float(&p[2], c); put_float(&p[3], d);
}

void glOrtho(double l, double r, double b, double t, double n, double f) {
    uint32_t *p = begin(GEXGL_ORTHO, 12, 0); if (!p) return;
    put_double(&p[0], l); put_double(&p[2], r); put_double(&p[4], b);
    put_double(&p[6], t); put_double(&p[8], n); put_double(&p[10], f);
}

void glColor3f(float r, float g, float b) { put_f4(GEXGL_COLOR4F, r, g, b, 1.0f); }
void glColor4f(float r, float g, float b, float a) { put_f4(GEXGL_COLOR4F, r, g, b, a); }
void glColor3fv(const float *v) { if (v) put_f4(GEXGL_COLOR4F, v[0], v[1], v[2], 1.0f); }
void glColor4fv(const float *v) { if (v) put_f4(GEXGL_COLOR4F, v[0], v[1], v[2], v[3]); }
void glColor3d(double r, double g, double b) { put_f4(GEXGL_COLOR4F, (float)r, (float)g, (float)b, 1.0f); }
void glColor4d(double r, double g, double b, double a) { put_f4(GEXGL_COLOR4F, (float)r, (float)g, (float)b, (float)a); }
/* The integer colour forms scale to [0, 1] as GL specifies: unsigned bytes by
   255, signed bytes by 127. */
void glColor3ub(unsigned char r, unsigned char g, unsigned char b) {
    put_f4(GEXGL_COLOR4F, r / 255.0f, g / 255.0f, b / 255.0f, 1.0f);
}
void glColor4ub(unsigned char r, unsigned char g, unsigned char b, unsigned char a) {
    put_f4(GEXGL_COLOR4F, r / 255.0f, g / 255.0f, b / 255.0f, a / 255.0f);
}
void glColor3ubv(const unsigned char *v) { if (v) glColor3ub(v[0], v[1], v[2]); }
void glColor4ubv(const unsigned char *v) { if (v) glColor4ub(v[0], v[1], v[2], v[3]); }

void glTexCoord2f(float s_, float t_) { put_f4(GEXGL_TEXCOORD4F, s_, t_, 0.0f, 1.0f); }
void glTexCoord3f(float s_, float t_, float r) { put_f4(GEXGL_TEXCOORD4F, s_, t_, r, 1.0f); }
void glTexCoord4f(float s_, float t_, float r, float q) { put_f4(GEXGL_TEXCOORD4F, s_, t_, r, q); }
void glTexCoord2fv(const float *v) { if (v) put_f4(GEXGL_TEXCOORD4F, v[0], v[1], 0.0f, 1.0f); }
void glTexCoord3fv(const float *v) { if (v) put_f4(GEXGL_TEXCOORD4F, v[0], v[1], v[2], 1.0f); }
void glTexCoord4fv(const float *v) { if (v) put_f4(GEXGL_TEXCOORD4F, v[0], v[1], v[2], v[3]); }
void glTexCoord2d(double s_, double t_) { put_f4(GEXGL_TEXCOORD4F, (float)s_, (float)t_, 0.0f, 1.0f); }
void glTexCoord2i(int s_, int t_) { put_f4(GEXGL_TEXCOORD4F, (float)s_, (float)t_, 0.0f, 1.0f); }

void glVertex2f(float x, float y) { glVertex3f(x, y, 0.0f); }
void glVertex2i(int x, int y) { glVertex3f((float)x, (float)y, 0.0f); }
void glVertex2d(double x, double y) { glVertex3f((float)x, (float)y, 0.0f); }
void glVertex2fv(const float *v) { if (v) glVertex3f(v[0], v[1], 0.0f); }
void glVertex3fv(const float *v) { if (v) glVertex3f(v[0], v[1], v[2]); }
void glVertex3d(double x, double y, double z) { glVertex3f((float)x, (float)y, (float)z); }
void glVertex3dv(const double *v) { if (v) glVertex3f((float)v[0], (float)v[1], (float)v[2]); }
void glVertex3i(int x, int y, int z) { glVertex3f((float)x, (float)y, (float)z); }
void glNormal3fv(const float *v) { if (v) glNormal3f(v[0], v[1], v[2]); }
void glNormal3d(double x, double y, double z) { glNormal3f((float)x, (float)y, (float)z); }

void glScalef(float x, float y, float z) {
    uint32_t *p = begin(GEXGL_SCALEF, 3, 0); if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y); put_float(&p[2], z);
}
void glScaled(double x, double y, double z) { glScalef((float)x, (float)y, (float)z); }
void glRotated(double a, double x, double y, double z) {
    glRotatef((float)a, (float)x, (float)y, (float)z);
}

static void put_enum_f(uint32_t opcode, unsigned target, unsigned pname, float v) {
    uint32_t *p = begin(opcode, 3, 0); if (!p) return;
    p[0] = target; p[1] = pname; put_float(&p[2], v);
}
void glTexEnvf(unsigned target, unsigned pname, float v) {
    put_enum_f(GEXGL_TEXENVF, target, pname, v);
}
void glTexEnvi(unsigned target, unsigned pname, int v) {
    uint32_t *p = begin(GEXGL_TEXENVI, 3, 0); if (!p) return;
    p[0] = target; p[1] = pname; p[2] = (uint32_t)v;
}
void glTexEnvfv(unsigned target, unsigned pname, const float *v) {
    /* Only the scalar parameters are forwarded as scalars; GL_TEXTURE_ENV_COLOR
       is the one vector form and takes four. */
    if (!v) return;
    if (pname == 0x2201u) { /* GL_TEXTURE_ENV_COLOR */
        uint32_t *p = begin(GEXGL_TEXENVF, 6, 0); if (!p) return;
        p[0] = target; p[1] = pname;
        for (unsigned i = 0; i < 4; ++i) put_float(&p[2 + i], v[i]);
        return;
    }
    put_enum_f(GEXGL_TEXENVF, target, pname, v[0]);
}

void glFogf(unsigned pname, float v) {
    uint32_t *p = begin(GEXGL_FOGF, 2, 0); if (!p) return;
    p[0] = pname; put_float(&p[1], v);
}
void glFogi(unsigned pname, int v) {
    uint32_t *p = begin(GEXGL_FOGI, 2, 0); if (!p) return;
    p[0] = pname; p[1] = (uint32_t)v;
}
void glFogfv(unsigned pname, const float *v) {
    if (!v) return;
    /* GL_FOG_COLOR is the only vector parameter; the rest are scalars. */
    const unsigned count = pname == 0x0B66u ? 4u : 1u; /* GL_FOG_COLOR */
    uint32_t *p = begin(GEXGL_FOGFV, 2 + count, 0); if (!p) return;
    p[0] = pname; p[1] = count;
    for (unsigned i = 0; i < count; ++i) put_float(&p[2 + i], v[i]);
}

void glClipPlane(unsigned plane, const double *equation) {
    if (!equation) return;
    uint32_t *p = begin(GEXGL_CLIP_PLANE, 9, 0); if (!p) return;
    p[0] = plane;
    for (unsigned i = 0; i < 4; ++i) put_double(&p[1 + i * 2], equation[i]);
}

void glAlphaFunc(unsigned func, float ref) {
    uint32_t *p = begin(GEXGL_ALPHA_FUNC, 2, 0); if (!p) return;
    p[0] = func; put_float(&p[1], ref);
}

void glRectf(float x1, float y1, float x2, float y2) {
    put_f4(GEXGL_RECTF, x1, y1, x2, y2);
}
void glRecti(int x1, int y1, int x2, int y2) {
    put_f4(GEXGL_RECTF, (float)x1, (float)y1, (float)x2, (float)y2);
}
void glRectd(double x1, double y1, double x2, double y2) {
    put_f4(GEXGL_RECTF, (float)x1, (float)y1, (float)x2, (float)y2);
}

void glRasterPos2f(float x, float y) { put_f4(GEXGL_RASTER_POS4F, x, y, 0.0f, 1.0f); }
void glRasterPos2i(int x, int y) { put_f4(GEXGL_RASTER_POS4F, (float)x, (float)y, 0.0f, 1.0f); }
void glRasterPos2d(double x, double y) { put_f4(GEXGL_RASTER_POS4F, (float)x, (float)y, 0.0f, 1.0f); }
void glRasterPos3f(float x, float y, float z) { put_f4(GEXGL_RASTER_POS4F, x, y, z, 1.0f); }
void glRasterPos3fv(const float *v) { if (v) put_f4(GEXGL_RASTER_POS4F, v[0], v[1], v[2], 1.0f); }

void glLineStipple(int factor, unsigned short pattern) {
    uint32_t *p = begin(GEXGL_LINE_STIPPLE, 2, 0); if (!p) return;
    p[0] = (uint32_t)factor; p[1] = pattern;
}

void glPushAttrib(unsigned mask) { CMD1(GEXGL_PUSH_ATTRIB, mask); }
void glPopAttrib(void) { CMD0(GEXGL_POP_ATTRIB); }

void glPixelZoom(float x, float y) {
    uint32_t *p = begin(GEXGL_PIXEL_ZOOM, 2, 0); if (!p) return;
    put_float(&p[0], x); put_float(&p[1], y);
}

void glLightModelfv(unsigned pname, const float *v) {
    if (!v) return;
    /* GL_LIGHT_MODEL_AMBIENT takes four; the rest are scalars. */
    const unsigned count = pname == 0x0B53u ? 4u : 1u; /* LIGHT_MODEL_AMBIENT */
    uint32_t *p = begin(GEXGL_LIGHT_MODELFV, 2 + count, 0); if (!p) return;
    p[0] = pname; p[1] = count;
    for (unsigned i = 0; i < count; ++i) put_float(&p[2 + i], v[i]);
}
void glLightModelf(unsigned pname, float v) { glLightModelfv(pname, &v); }
void glLightModeli(unsigned pname, int v) { const float f = (float)v; glLightModelfv(pname, &f); }

void glMaterialf(unsigned face, unsigned pname, float v) {
    put_enum_f(GEXGL_MATERIALF, face, pname, v);
}
void glLightf(unsigned light, unsigned pname, float v) {
    put_enum_f(GEXGL_LIGHTF, light, pname, v);
}
void glLighti(unsigned light, unsigned pname, int v) { glLightf(light, pname, (float)v); }
void glMateriali(unsigned face, unsigned pname, int v) { glMaterialf(face, pname, (float)v); }

static void put_matrix(uint32_t opcode, const float *m) {
    if (!m) return;
    uint32_t *p = begin(opcode, 16, 0); if (!p) return;
    for (unsigned i = 0; i < 16; ++i) put_float(&p[i], m[i]);
}
void glMultMatrixf(const float *m) { put_matrix(GEXGL_MULT_MATRIXF, m); }
void glLoadMatrixf(const float *m) { put_matrix(GEXGL_LOAD_MATRIXF, m); }
void glMultMatrixd(const double *m) {
    if (!m) return;
    float f[16];
    for (unsigned i = 0; i < 16; ++i) f[i] = (float)m[i];
    put_matrix(GEXGL_MULT_MATRIXF, f);
}
void glLoadMatrixd(const double *m) {
    if (!m) return;
    float f[16];
    for (unsigned i = 0; i < 16; ++i) f[i] = (float)m[i];
    put_matrix(GEXGL_LOAD_MATRIXF, f);
}

void glColorMaterial(unsigned face, unsigned mode) {
    uint32_t *p = begin(GEXGL_COLOR_MATERIAL, 2, 0); if (!p) return;
    p[0] = face; p[1] = mode;
}

/* glPolygonStipple: the 32x32 pattern, as 32 words.
 *
 * Hand-written rather than generated because the generator covers GL 4.4 core
 * and this is compatibility-only. The pattern's registry length is
 * COMPSIZE(), which no generated policy resolves, but it is a fixed 128 bytes
 * -- 32 rows of 4 -- so there is nothing to compute.
 *
 * Missing entirely, this was not a silently absent feature: libGL exports a
 * fixed set of symbols, so an application that calls glPolygonStipple failed
 * to resolve it and did not start at all. */
void glPolygonStipple(const unsigned char *mask) {
    if (!mask) return;
    uint32_t *p = begin(GEXGL_POLYGON_STIPPLE, 32, 0);
    if (!p) return;
    for (size_t row = 0; row < 32; ++row) {
        /* Big-endian within each row, as glPolygonStipple defines it: the
           first byte holds the leftmost eight pixels, most significant bit
           first. Packed here so the host sees one word per row with bit 31 as
           the leftmost pixel, which is the form Gallium's pipe_poly_stipple
           uses. */
        p[row] = ((uint32_t)mask[row * 4 + 0] << 24) |
                 ((uint32_t)mask[row * 4 + 1] << 16) |
                 ((uint32_t)mask[row * 4 + 2] << 8) |
                 (uint32_t)mask[row * 4 + 3];
    }
}

/* The host's own list names are handed straight back. Display lists are the
   only object here the guest does not allocate names for, and glGenLists is
   their only source, so guest and host cannot disagree about them. */
unsigned glGenLists(int range) {
    CMD1(GEXGL_GEN_LISTS, (unsigned)range);
    flush();
    return reply_u32();
}

/* Selection mode.
 *
 * The one remaining piece of ordinary GL the guest could not call at all, and
 * `glpuzzle` picks its pieces with it. The records are written on the host
 * side and read on the guest's, so glSelectBuffer keeps the client pointer
 * here and tells the host only how large its own buffer must be;
 * glRenderMode then brings back both the return value and the buffer, the way
 * glReadPixels brings back pixels.
 *
 * Copying the whole buffer rather than just the hits a record count implies:
 * a hit record is a variable number of words -- four plus one per name on the
 * stack -- so the count alone does not say how much to copy, and the buffer
 * is small.
 */
static unsigned *select_buffer;
static int select_buffer_size;

void glSelectBuffer(int size, unsigned *buffer) {
    select_buffer = buffer;
    select_buffer_size = size > 0 ? size : 0;
    CMD1(GEXGL_SELECT_BUFFER, (uint32_t)select_buffer_size);
}

int glRenderMode(unsigned mode) {
    CMD1(GEXGL_RENDER_MODE, mode);
    flush();
    const uint32_t hits = reply_u32();
    uint32_t n = 0;
    const uint8_t *records = reply_blob(&n);
    if (records && select_buffer && select_buffer_size > 0) {
        size_t words = n / sizeof(unsigned);
        if (words > (size_t)select_buffer_size) words = (size_t)select_buffer_size;
        memcpy(select_buffer, records, words * sizeof(unsigned));
    }
    return (int)hits;
}

void glInitNames(void) { CMD0(GEXGL_INIT_NAMES); }
void glLoadName(unsigned name) { CMD1(GEXGL_LOAD_NAME, name); }
void glPushName(unsigned name) { CMD1(GEXGL_PUSH_NAME, name); }
void glPopName(void) { CMD0(GEXGL_POP_NAME); }

/* GLX, exported from libGL.so.1. After the EGL above, which it is built on.
 */
#include "gexgl-glx.inc"

/* Registry command aliases; see tools/gen_gl_extensions.py. Last, so every
   alias target above is already declared. */
#include "gl-aliases-generated.inc"
