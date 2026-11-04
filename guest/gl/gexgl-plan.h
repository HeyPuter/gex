/* Wire plans (scripts/gl/gen_gl_compat.py): what both ends of a generated
 * command agree on. A plan is a return token then one token per parameter;
 * this parses them and sizes their arrays, for the guest's interpreter
 * (gexgl.c) and the host decoders (ports/mesa/src/gexgl-compat-host.h), so the
 * size an encoder sends is the one a decoder checks. C and C++. */
#ifndef GEXGL_PLAN_H
#define GEXGL_PLAN_H

#include <stddef.h>
#include <stdint.h>

#include "gexgl-pixel.h"
#include "gl-pname-counts.h"

#define GEXGEN_MAX_PARAMS 24
/* No single array is larger than this on the wire. */
#define GEXGEN_MAX_BYTES ((size_t)1 << 28)

struct gexgen_tok {
    char kind;     /* w f q: scalars; i j: input arrays; o: output */
    unsigned size; /* element bytes, for arrays */
    char spec;     /* n p e z L s S X B T */
    int arg[5];    /* parameter indices (-1 absent) or N / multiplier */
};

/* A non-negative element count from a signed parameter, times `mult`,
 * saturating at what the wire can carry. */
static inline size_t gexgen_count(long long value, long long mult) {
    if (value <= 0 || mult <= 0) return 0;
    if ((unsigned long long)value > GEXGEN_MAX_BYTES / (unsigned long long)mult) return GEXGEN_MAX_BYTES;
    return (size_t)(value * mult);
}

/* glCallLists' element types. */
static inline size_t gexgen_type_bytes(unsigned type) {
    switch (type) {
    case 0x1400: case 0x1401: return 1; /* BYTE, UNSIGNED_BYTE */
    case 0x1402: case 0x1403: case 0x1407: return 2; /* SHORT, UNSIGNED_SHORT, 2_BYTES */
    case 0x1408: return 3; /* 3_BYTES */
    case 0x1404: case 0x1405: case 0x1406: case 0x1409: return 4; /* INT, UNSIGNED_INT, FLOAT, 4_BYTES */
    default: return 0;
    }
}

/* A glBitmap's bytes under the unpack store: rows of (row length or width)
 * bits, each padded to the alignment, after the skipped rows and pixels. */
static inline size_t gexgen_bitmap_bytes_store(long long width, long long height,
                                               const struct gexgl_pixel_store *s) {
    if (width <= 0 || height <= 0 || width > 1 << 20 || height > 1 << 20) return 0;
    size_t px = s->row_length ? s->row_length : (size_t)width;
    size_t row = (px + 7) / 8, a = s->alignment ? s->alignment : 1;
    row = (row + a - 1) / a * a;
    return (s->skip_rows + (size_t)height) * row + (s->skip_pixels + 7) / 8;
}

/* Parse one token at `p` into `t`; returns past it, or NULL at the end. */
static inline const char *gexgen_parse(const char *p, struct gexgen_tok *t) {
    while (*p == ' ') p++;
    if (!*p) return NULL;
    t->kind = *p++;
    t->size = 0;
    t->spec = 0;
    for (int i = 0; i < 5; i++) t->arg[i] = -1;
    if (t->kind == 'i' || t->kind == 'j' || t->kind == 'o') {
        while (*p >= '0' && *p <= '9') t->size = t->size * 10 + (unsigned)(*p++ - '0');
        if (*p == ':') p++;
        t->spec = *p++;
        int n = 0;
        while (*p && *p != ' ' && n < 5) {
            if (*p == '_') { p++; n++; }
            else if (*p >= '0' && *p <= '9') {
                int v = 0;
                while (*p >= '0' && *p <= '9') v = v * 10 + (*p++ - '0');
                t->arg[n++] = v;
            } else if (*p != ',' && *p != '*') {
                p++;
            }
            if (*p == ',' || *p == '*') p++;
        }
        /* p<i> without a multiplier multiplies by one. */
        if (t->spec == 'p' && t->arg[1] < 0) t->arg[1] = 1;
    }
    return p;
}

#endif
