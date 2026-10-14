/* The gex WebGPU wire: webgpu.h calls as a byte stream, from a client (the
 * guest's webgpu.h, wire-client.c) to a server that makes them on a real
 * webgpu.h (wire-server.c). docs/d3d-webgpu.md describes the protocol.
 *
 * The stream is little-endian words: a message is commands, each
 * {u32 op, u32 byte length, arguments}; the reply is records, each
 * {u32 kind, u32 byte length, payload}. Objects are u32 ids the client
 * chooses, so creating one needs no round trip. Everything here is plain
 * C, so that the client builds inside a Wine PE DLL for any architecture
 * and the server against Dawn or emdawnwebgpu. */

#ifndef GEXGPU_WIRE_H
#define GEXGPU_WIRE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "webgpu.h"
#include "gen/wire-gen.h"

#define GEXGPU_WIRE_VERSION 1

/* Server -> client record kinds. */
enum gexgpu_record
{
    /* The result of the call the message ended with. */
    GEXGPU_RECORD_REPLY = 1,
    /* A callback's arguments: u32 cookie, then the callback's own. */
    GEXGPU_RECORD_EVENT = 2,
};

/* Ops beyond webgpu.h's, after the generated ones. */
enum gexgpu_special_op
{
    /* u64 timeout in ns, u32 count, u32 cookies[count]: wait until one of
     * the futures completes, then reply. */
    GEXGPU_OP_WAIT = GEXGPU_OP_COUNT,
    /* Process events; reply. */
    GEXGPU_OP_PROCESS_EVENTS,
};

struct gexgpu_writer
{
    uint8_t *data;
    size_t size, capacity;
    bool error;
};

static inline void gexgpu_w_reserve(struct gexgpu_writer *w, size_t n)
{
    size_t capacity;
    uint8_t *data;

    if (w->size + n <= w->capacity)
        return;
    capacity = w->capacity ? w->capacity : 4096;
    while (capacity < w->size + n)
        capacity *= 2;
    if (!(data = realloc(w->data, capacity)))
    {
        w->error = true;
        return;
    }
    w->data = data;
    w->capacity = capacity;
}

static inline void gexgpu_w_raw(struct gexgpu_writer *w, const void *p, size_t n)
{
    gexgpu_w_reserve(w, n);
    if (w->error)
        return;
    memcpy(w->data + w->size, p, n);
    w->size += n;
}

static inline void gexgpu_w_u32(struct gexgpu_writer *w, uint32_t v)
{
    gexgpu_w_raw(w, &v, sizeof(v));
}

static inline void gexgpu_w_u64(struct gexgpu_writer *w, uint64_t v)
{
    gexgpu_w_raw(w, &v, sizeof(v));
}

static inline void gexgpu_w_f32(struct gexgpu_writer *w, float v)
{
    gexgpu_w_raw(w, &v, sizeof(v));
}

static inline void gexgpu_w_f64(struct gexgpu_writer *w, double v)
{
    gexgpu_w_raw(w, &v, sizeof(v));
}

/* Bytes, length-prefixed and padded to a word. */
static inline void gexgpu_w_blob(struct gexgpu_writer *w, const void *p, size_t n)
{
    static const uint8_t zero[4];

    gexgpu_w_u32(w, (uint32_t)n);
    if (n)
        gexgpu_w_raw(w, p, n);
    if (n & 3)
        gexgpu_w_raw(w, zero, 4 - (n & 3));
}

/* A string view: ~0u for a null one. */
static inline void gexgpu_w_string(struct gexgpu_writer *w, WGPUStringView s)
{
    size_t n;

    if (!s.data)
    {
        gexgpu_w_u32(w, ~0u);
        return;
    }
    n = s.length == WGPU_STRLEN ? strlen(s.data) : s.length;
    gexgpu_w_blob(w, s.data, n);
}

/* Patches the length of the command or record that starts at "start". */
static inline void gexgpu_w_patch_length(struct gexgpu_writer *w, size_t start)
{
    uint32_t length = (uint32_t)(w->size - start);

    if (!w->error)
        memcpy(w->data + start + 4, &length, sizeof(length));
}

struct gexgpu_reader
{
    const uint8_t *data;
    size_t size, pos;
    bool error;
    /* Where strings and arrays go: the server's per-message arena, or the
     * client's malloc() for results the caller frees. */
    void *(*alloc)(struct gexgpu_reader *r, size_t size);
    void *alloc_context;
};

static inline void gexgpu_r_raw(struct gexgpu_reader *r, void *p, size_t n)
{
    if (r->error || r->size - r->pos < n)
    {
        r->error = true;
        memset(p, 0, n);
        return;
    }
    memcpy(p, r->data + r->pos, n);
    r->pos += n;
}

static inline uint32_t gexgpu_r_u32(struct gexgpu_reader *r)
{
    uint32_t v;

    gexgpu_r_raw(r, &v, sizeof(v));
    return v;
}

static inline uint64_t gexgpu_r_u64(struct gexgpu_reader *r)
{
    uint64_t v;

    gexgpu_r_raw(r, &v, sizeof(v));
    return v;
}

static inline float gexgpu_r_f32(struct gexgpu_reader *r)
{
    float v;

    gexgpu_r_raw(r, &v, sizeof(v));
    return v;
}

static inline double gexgpu_r_f64(struct gexgpu_reader *r)
{
    double v;

    gexgpu_r_raw(r, &v, sizeof(v));
    return v;
}

static inline void *gexgpu_r_alloc(struct gexgpu_reader *r, size_t size)
{
    void *p;

    if (r->error)
        return NULL;
    if (!(p = r->alloc(r, size ? size : 1)))
        r->error = true;
    return p;
}

/* A blob's bytes, copied out with a NUL after them. */
static inline void *gexgpu_r_blob(struct gexgpu_reader *r, size_t *size)
{
    uint32_t n = gexgpu_r_u32(r);
    uint8_t *p;

    *size = 0;
    if (r->error || r->size - r->pos < ((n + 3) & ~3u))
    {
        r->error = true;
        return NULL;
    }
    if (!(p = gexgpu_r_alloc(r, (size_t)n + 1)))
        return NULL;
    memcpy(p, r->data + r->pos, n);
    p[n] = 0;
    r->pos += (n + 3) & ~3u;
    *size = n;
    return p;
}

static inline WGPUStringView gexgpu_r_string(struct gexgpu_reader *r)
{
    WGPUStringView s = {NULL, 0};
    size_t n;

    if (r->error)
        return s;
    if (r->size - r->pos >= 4 && !memcmp(r->data + r->pos, "\xff\xff\xff\xff", 4))
    {
        r->pos += 4;
        s.length = WGPU_STRLEN;
        return s;
    }
    s.data = gexgpu_r_blob(r, &n);
    s.length = n;
    return s;
}

#endif
