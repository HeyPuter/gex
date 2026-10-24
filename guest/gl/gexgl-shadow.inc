/* Fixed-function state answered here, without a round trip.
 *
 * Half-Life asks glGetFloatv(GL_MODELVIEW_MATRIX), glGetFloatv(
 * GL_PROJECTION_MATRIX) and glIsEnabled(GL_FOG) several times a frame. Each is
 * a submit the guest waits for: everything queued before it runs on the host
 * first, so the guest's frame and the GL worker's frame happen one after the
 * other instead of side by side. In the page that held c1a0 at 40 frames/s
 * with neither thread busy.
 *
 * The answers follow from commands this library has seen go by, so they are
 * kept here: the modelview and projection stacks, computed as Mesa computes
 * them (src/mesa/math/m_matrix.c, the same float arithmetic in the same
 * order), and the server-side enables of a short list of capabilities. A
 * value is only ever known from the host's own answer or from commands
 * applied to such a value; anything this cannot follow -- a display list
 * that changed state, glPopAttrib, a named-matrix (EXT) or fixed-point (OES)
 * entry point, a matrix mode it does not track -- forgets what it would have
 * changed, and the next query asks the host again.
 *
 * GEXGL_NO_QUERY_CACHE=1 asks the host every time. GEXGL_SHADOW_CHECK=1 asks
 * it every time too and says on stderr whenever the answer kept here differs.
 */

#include <math.h>

/* i386's x87 keeps float arithmetic at 80 bits; the host rounds every step
   to float, so the arithmetic here does too. (Which is also why what comes
   in from outside this file comes by pointer: a float argument would be
   passed the SSE way to code that passes it the x87 way.) */
#if defined(__i386__) && defined(__GNUC__) && !defined(__clang__)
#pragma GCC push_options
#pragma GCC target("sse2", "fpmath=sse")
#define SHADOW_POP_OPTIONS
#endif

#define SHADOW_DEPTH 32 /* Mesa's MAX_MODELVIEW_STACK_DEPTH and _PROJECTION_ */

struct shadow_matrix {
    float m[16];
    int known;
};

static struct {
    /* 0 modelview, 1 projection, 2 a stack not kept here, -1 not known. */
    int mode;
    /* Each stack's depth, -1 once it is not known. */
    int depth[2];
    struct shadow_matrix stack[2][SHADOW_DEPTH];
    int in_begin;
    /* Inside glNewList: 1 compiling only (GL_COMPILE), 2 compiling and
       executing; what the list being made changes, in `list_touches`. */
    int listing;
    unsigned list_touches;
    /* What any display list made so far changes (SHADOW_TOUCH_*). */
    unsigned lists_touch;
    /* Matrix modes saved by glPushAttrib(GL_TRANSFORM_BIT), -2 for a push
       without it; `attrib_depth` -1 once not known. */
    int attrib_mode[16];
    int attrib_depth;
    struct { unsigned cap; unsigned char on; } enables[24];
    unsigned n_enables;
} shadow = { .mode = -1, .depth = { -1, -1 }, .attrib_depth = -1 };

enum { SHADOW_TOUCH_MATRIX = 1, SHADOW_TOUCH_ENABLE = 2 };

static int shadow_off(void) {
    static int off = -1;
    if (off < 0) off = getenv("GEXGL_NO_QUERY_CACHE") ? 1 : 0;
    return off;
}

static int shadow_check(void) {
    static int on = -1;
    if (on < 0) on = getenv("GEXGL_SHADOW_CHECK") ? 1 : 0;
    return on;
}

static void shadow_identity(float *m) {
    for (int i = 0; i < 16; i++) m[i] = (i % 5 == 0) ? 1.0f : 0.0f;
}

/* A new host context: Mesa's initial state, which is known. */
static void shadow_context_made(void) {
    shadow.mode = 0;
    for (int s = 0; s < 2; s++) {
        shadow.depth[s] = 0;
        shadow_identity(shadow.stack[s][0].m);
        shadow.stack[s][0].known = 1;
    }
    shadow.in_begin = 0;
    shadow.listing = 0;
    shadow.attrib_depth = 0;
    shadow.n_enables = 0;
}

static void shadow_forget_enables(void) { shadow.n_enables = 0; }

/* Nothing about the matrices is known any more. */
static void shadow_forget_matrices(void) {
    shadow.mode = -1;
    shadow.depth[0] = shadow.depth[1] = -1;
    shadow.attrib_depth = -1;
}

/* A command that changes `what` and that this does not follow. While a
   display list is compiled it is only noted: the list changes it when called. */
static void shadow_unknown(unsigned what) {
    if (shadow.listing) {
        shadow.list_touches |= what;
        if (shadow.listing == 1) return;
    }
    if (what & SHADOW_TOUCH_MATRIX) shadow_forget_matrices();
    if (what & SHADOW_TOUCH_ENABLE) shadow_forget_enables();
}

/* Whether a command is carried out now: not between glBegin and glEnd (an
   error there), and not into a list being compiled only. A command that is
   only compiled is still noted as one the list carries. */
static int shadow_applies(unsigned what) {
    if (shadow.listing) {
        shadow.list_touches |= what;
        if (shadow.listing == 1) return 0;
    }
    return !shadow.in_begin;
}

/* The top of the current stack, when it is one kept here and its depth is
   known; NULL otherwise. */
static struct shadow_matrix *shadow_top(void) {
    if (shadow.mode != 0 && shadow.mode != 1) return NULL;
    const int d = shadow.depth[shadow.mode];
    return d < 0 ? NULL : &shadow.stack[shadow.mode][d];
}

/* A command that changes the current matrix: the top it changes, if known.
   With the mode not known, both kept stacks may be the current one. */
static struct shadow_matrix *shadow_changing(void) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return NULL;
    if (shadow.mode == -1) {
        for (int s = 0; s < 2; s++)
            if (shadow.depth[s] >= 0) shadow.stack[s][shadow.depth[s]].known = 0;
        return NULL;
    }
    struct shadow_matrix *t = shadow_top();
    return t && t->known ? t : NULL;
}

/* Mesa's matmul4: product = a * b, column-major, product may be a. */
static void shadow_matmul4(float *product, const float *a, const float *b) {
#define SA(row, col) a[(col << 2) + row]
#define SB(row, col) b[(col << 2) + row]
#define SP(row, col) product[(col << 2) + row]
    for (int i = 0; i < 4; i++) {
        const float ai0 = SA(i, 0), ai1 = SA(i, 1), ai2 = SA(i, 2), ai3 = SA(i, 3);
        SP(i, 0) = ai0 * SB(0, 0) + ai1 * SB(1, 0) + ai2 * SB(2, 0) + ai3 * SB(3, 0);
        SP(i, 1) = ai0 * SB(0, 1) + ai1 * SB(1, 1) + ai2 * SB(2, 1) + ai3 * SB(3, 1);
        SP(i, 2) = ai0 * SB(0, 2) + ai1 * SB(1, 2) + ai2 * SB(2, 2) + ai3 * SB(3, 2);
        SP(i, 3) = ai0 * SB(0, 3) + ai1 * SB(1, 3) + ai2 * SB(2, 3) + ai3 * SB(3, 3);
    }
#undef SA
#undef SB
#undef SP
}

static void shadow_mult(const float *m) {
    struct shadow_matrix *t = shadow_changing();
    if (t) shadow_matmul4(t->m, t->m, m);
}

static void shadow_load(const float *m) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return;
    if (shadow.mode == -1) { (void)shadow_changing(); return; }
    struct shadow_matrix *t = shadow_top();
    if (!t) return;
    memcpy(t->m, m, sizeof(t->m));
    t->known = 1;
}

static void shadow_transpose(float *to, const float *from) {
    for (int r = 0; r < 4; r++)
        for (int c = 0; c < 4; c++) to[c * 4 + r] = from[r * 4 + c];
}

static void shadow_matrix_mode(unsigned mode) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return;
    switch (mode) {
    case 0x1700u: shadow.mode = 0; break; /* GL_MODELVIEW */
    case 0x1701u: shadow.mode = 1; break; /* GL_PROJECTION */
    case 0x1702u: shadow.mode = 2; break; /* GL_TEXTURE */
    default:
        /* GL_COLOR, GL_MATRIXi_ARB: valid or an error depending on the
           context's extensions; GL_TEXTUREi: ignored by Mesa. */
        if (mode < 0x84C0u || mode > 0x84DFu) shadow.mode = -1;
        break;
    }
}

static void shadow_push(void) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return;
    if (shadow.mode == 2) return;
    if (shadow.mode == -1) { shadow.depth[0] = shadow.depth[1] = -1; return; }
    int *d = &shadow.depth[shadow.mode];
    if (*d < 0) return;
    if (*d + 1 >= SHADOW_DEPTH) return; /* GL_STACK_OVERFLOW: no change */
    shadow.stack[shadow.mode][*d + 1] = shadow.stack[shadow.mode][*d];
    ++*d;
}

static void shadow_pop(void) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return;
    if (shadow.mode == 2) return;
    if (shadow.mode == -1) { shadow.depth[0] = shadow.depth[1] = -1; return; }
    int *d = &shadow.depth[shadow.mode];
    if (*d > 0) --*d; /* at 0, GL_STACK_UNDERFLOW: no change */
}

static void shadow_rotate(const float *axyz) {
    float angle = axyz[0], x = axyz[1], y = axyz[2], z = axyz[3];
    struct shadow_matrix *t = shadow_changing();
    if (!t || angle == 0.0f) return;
    float m[16];
    const float s = sinf(angle * M_PI / 180.0);
    const float c = cosf(angle * M_PI / 180.0);
    int optimized = 0;
    shadow_identity(m);
#define SM(row, col) m[col * 4 + row]
    if (x == 0.0f) {
        if (y == 0.0f) {
            if (z != 0.0f) {
                optimized = 1;
                SM(0, 0) = c;
                SM(1, 1) = c;
                if (z < 0.0f) { SM(0, 1) = s; SM(1, 0) = -s; }
                else { SM(0, 1) = -s; SM(1, 0) = s; }
            }
        } else if (z == 0.0f) {
            optimized = 1;
            SM(0, 0) = c;
            SM(2, 2) = c;
            if (y < 0.0f) { SM(0, 2) = -s; SM(2, 0) = s; }
            else { SM(0, 2) = s; SM(2, 0) = -s; }
        }
    } else if (y == 0.0f) {
        if (z == 0.0f) {
            optimized = 1;
            SM(1, 1) = c;
            SM(2, 2) = c;
            if (x < 0.0f) { SM(1, 2) = s; SM(2, 1) = -s; }
            else { SM(1, 2) = -s; SM(2, 1) = s; }
        }
    }
    if (!optimized) {
        const float mag = sqrtf(x * x + y * y + z * z);
        if (mag <= 1.0e-4f) return;
        x /= mag;
        y /= mag;
        z /= mag;
        const float xx = x * x, yy = y * y, zz = z * z;
        const float xy = x * y, yz = y * z, zx = z * x;
        const float xs = x * s, ys = y * s, zs = z * s;
        const float one_c = 1.0f - c;
        SM(0, 0) = (one_c * xx) + c;
        SM(0, 1) = (one_c * xy) - zs;
        SM(0, 2) = (one_c * zx) + ys;
        SM(1, 0) = (one_c * xy) + zs;
        SM(1, 1) = (one_c * yy) + c;
        SM(1, 2) = (one_c * yz) - xs;
        SM(2, 0) = (one_c * zx) - ys;
        SM(2, 1) = (one_c * yz) + xs;
        SM(2, 2) = (one_c * zz) + c;
    }
#undef SM
    shadow_matmul4(t->m, t->m, m);
}

static void shadow_translate(const float *xyz) {
    const float x = xyz[0], y = xyz[1], z = xyz[2];
    struct shadow_matrix *t = shadow_changing();
    if (!t) return;
    float *m = t->m;
    m[12] = m[0] * x + m[4] * y + m[8] * z + m[12];
    m[13] = m[1] * x + m[5] * y + m[9] * z + m[13];
    m[14] = m[2] * x + m[6] * y + m[10] * z + m[14];
    m[15] = m[3] * x + m[7] * y + m[11] * z + m[15];
}

static void shadow_scale(const float *xyz) {
    const float x = xyz[0], y = xyz[1], z = xyz[2];
    struct shadow_matrix *t = shadow_changing();
    if (!t) return;
    float *m = t->m;
    m[0] *= x; m[4] *= y; m[8] *= z;
    m[1] *= x; m[5] *= y; m[9] *= z;
    m[2] *= x; m[6] *= y; m[10] *= z;
    m[3] *= x; m[7] *= y; m[11] *= z;
}

/* glFrustum and glOrtho: Mesa rounds the six values to float, then checks
   them (an invalid set changes nothing). */
static void shadow_frustum(const double *v) {
    const float l = (float)v[0], r = (float)v[1], b = (float)v[2], t = (float)v[3], n = (float)v[4], f = (float)v[5];
    if (n <= 0.0f || f <= 0.0f || n == f || l == r || t == b) {
        (void)shadow_applies(SHADOW_TOUCH_MATRIX);
        return;
    }
    struct shadow_matrix *top = shadow_changing();
    if (!top) return;
    float m[16];
    const float x = (2.0f * n) / (r - l);
    const float y = (2.0f * n) / (t - b);
    const float a = (r + l) / (r - l);
    const float bb = (t + b) / (t - b);
    const float c = -(f + n) / (f - n);
    const float d = -(2.0f * f * n) / (f - n);
#define SM(row, col) m[col * 4 + row]
    SM(0, 0) = x;    SM(0, 1) = 0.0f; SM(0, 2) = a;     SM(0, 3) = 0.0f;
    SM(1, 0) = 0.0f; SM(1, 1) = y;    SM(1, 2) = bb;    SM(1, 3) = 0.0f;
    SM(2, 0) = 0.0f; SM(2, 1) = 0.0f; SM(2, 2) = c;     SM(2, 3) = d;
    SM(3, 0) = 0.0f; SM(3, 1) = 0.0f; SM(3, 2) = -1.0f; SM(3, 3) = 0.0f;
#undef SM
    shadow_matmul4(top->m, top->m, m);
}

static void shadow_ortho(const double *v) {
    const float l = (float)v[0], r = (float)v[1], b = (float)v[2], t = (float)v[3], n = (float)v[4], f = (float)v[5];
    if (l == r || b == t || n == f) {
        (void)shadow_applies(SHADOW_TOUCH_MATRIX);
        return;
    }
    struct shadow_matrix *top = shadow_changing();
    if (!top) return;
    float m[16];
#define SM(row, col) m[col * 4 + row]
    SM(0, 0) = 2.0f / (r - l); SM(0, 1) = 0.0f; SM(0, 2) = 0.0f; SM(0, 3) = -(r + l) / (r - l);
    SM(1, 0) = 0.0f; SM(1, 1) = 2.0f / (t - b); SM(1, 2) = 0.0f; SM(1, 3) = -(t + b) / (t - b);
    SM(2, 0) = 0.0f; SM(2, 1) = 0.0f; SM(2, 2) = -2.0f / (f - n); SM(2, 3) = -(f + n) / (f - n);
    SM(3, 0) = 0.0f; SM(3, 1) = 0.0f; SM(3, 2) = 0.0f; SM(3, 3) = 1.0f;
#undef SM
    shadow_matmul4(top->m, top->m, m);
}

/* The capabilities kept: server-side, one per context (no texture unit, no
   index, no client array state), valid in every desktop compatibility
   context. */
static int shadow_cap_kept(unsigned cap) {
    switch (cap) {
    case 0x0B60u: /* GL_FOG */
    case 0x0BE2u: /* GL_BLEND */
    case 0x0B71u: /* GL_DEPTH_TEST */
    case 0x0B44u: /* GL_CULL_FACE */
    case 0x0BC0u: /* GL_ALPHA_TEST */
    case 0x0C11u: /* GL_SCISSOR_TEST */
    case 0x0B90u: /* GL_STENCIL_TEST */
    case 0x0B50u: /* GL_LIGHTING */
    case 0x0B57u: /* GL_COLOR_MATERIAL */
    case 0x0BA1u: /* GL_NORMALIZE */
    case 0x0BD0u: /* GL_DITHER */
    case 0x0B20u: /* GL_LINE_SMOOTH */
    case 0x8037u: /* GL_POLYGON_OFFSET_FILL */
    case 0x809Du: /* GL_MULTISAMPLE */
        return 1;
    default:
        return 0;
    }
}

static void shadow_set_enabled(unsigned cap, unsigned char on);

static void shadow_enable(unsigned cap, int on) {
    if (!shadow_applies(SHADOW_TOUCH_ENABLE)) return;
    if (shadow_cap_kept(cap)) shadow_set_enabled(cap, (unsigned char)(on != 0));
}

static void shadow_push_attrib(unsigned mask) {
    if (!shadow_applies(SHADOW_TOUCH_MATRIX)) return;
    if (shadow.attrib_depth < 0) return;
    if (shadow.attrib_depth >= 16) { shadow.attrib_depth = -1; return; }
    shadow.attrib_mode[shadow.attrib_depth++] = (mask & 0x1000u) ? shadow.mode : -2; /* GL_TRANSFORM_BIT */
}

/* glPopAttrib: the enables of whichever groups were pushed come back, and
   with GL_TRANSFORM_BIT the matrix mode does. */
static void shadow_pop_attrib(void) {
    if (shadow.listing) {
        shadow.list_touches |= SHADOW_TOUCH_MATRIX | SHADOW_TOUCH_ENABLE;
        if (shadow.listing == 1) return;
    }
    if (shadow.in_begin) return;
    shadow_forget_enables();
    if (shadow.attrib_depth < 0) { shadow.mode = -1; return; }
    if (shadow.attrib_depth == 0) return; /* GL_STACK_UNDERFLOW */
    const int saved = shadow.attrib_mode[--shadow.attrib_depth];
    if (saved != -2) shadow.mode = saved;
}

static void shadow_new_list(unsigned mode) {
    if (shadow.in_begin || shadow.listing) return;
    shadow.listing = mode == 0x1301u ? 2 : mode == 0x1300u ? 1 : 0; /* COMPILE_AND_EXECUTE, COMPILE */
    shadow.list_touches = 0;
}

static void shadow_end_list(void) {
    if (!shadow.listing) return;
    shadow.lists_touch |= shadow.list_touches;
    shadow.listing = 0;
}

/* glCallList(s): whatever a list made so far changes. */
static void shadow_call_lists(void) {
    if (shadow.lists_touch) shadow_unknown(shadow.lists_touch);
}

/* A generated entry point (gexgen_call), by its opcode: the ones that change
   what is kept here. */
static void shadow_generated(uint32_t opcode, const uint64_t *v) {
    float f[16];
    switch (opcode) {
    case 9699u: /* glLoadMatrixf */
    case 9707u: /* glLoadTransposeMatrixf */
    case 9806u: /* glMultMatrixf */
    case 9811u: { /* glMultTransposeMatrixf */
        const float *m = (const float *)(uintptr_t)v[0];
        if (!m) return;
        if (opcode == 9707u || opcode == 9811u) shadow_transpose(f, m);
        else memcpy(f, m, sizeof(f));
        if (opcode == 9699u || opcode == 9707u) shadow_load(f);
        else shadow_mult(f);
        return;
    }
    case 9698u: /* glLoadMatrixd */
    case 9705u: /* glLoadTransposeMatrixd */
    case 9805u: /* glMultMatrixd */
    case 9809u: { /* glMultTransposeMatrixd */
        const double *m = (const double *)(uintptr_t)v[0];
        if (!m) return;
        float g[16];
        for (int i = 0; i < 16; i++) g[i] = (float)m[i];
        if (opcode == 9705u || opcode == 9809u) shadow_transpose(f, g);
        else memcpy(f, g, sizeof(f));
        if (opcode == 9698u || opcode == 9705u) shadow_load(f);
        else shadow_mult(f);
        return;
    }
    case 10482u: { /* glRotated */
        double d[4];
        memcpy(d, v, sizeof(d));
        const float a[4] = { (float)d[0], (float)d[1], (float)d[2], (float)d[3] };
        shadow_rotate(a);
        return;
    }
    case 10508u: { /* glScalef */
        float s[3];
        for (int i = 0; i < 3; i++) { uint32_t b = (uint32_t)v[i]; memcpy(&s[i], &b, 4); }
        shadow_scale(s);
        return;
    }
    case 10507u: { /* glScaled */
        double d[3];
        memcpy(d, v, sizeof(d));
        const float s[3] = { (float)d[0], (float)d[1], (float)d[2] };
        shadow_scale(s);
        return;
    }
    case 10071u: { /* glOrtho */
        double d[6];
        memcpy(d, v, sizeof(d));
        shadow_ortho(d);
        return;
    }
    case 10367u: shadow_push_attrib((uint32_t)v[0]); return; /* glPushAttrib */
    case 10162u: shadow_pop_attrib(); return;                  /* glPopAttrib */
    case 8369u: shadow_call_lists(); return;                   /* glCallLists */
    /* Named-matrix (EXT_direct_state_access) and fixed-point (OES) forms. */
    case 8997u: case 8999u: case 9701u: case 9709u: case 9762u: case 9770u:
    case 9772u: case 9773u: case 9774u: case 9775u: case 9780u: case 9781u:
    case 9782u: case 9783u: case 9784u: case 9785u: case 9786u: case 9787u:
    case 9788u: case 9789u: case 9790u: case 9791u: case 9792u: case 9808u:
    case 9813u: case 10073u: case 10075u: case 10485u: case 10510u: case 10886u:
        shadow_unknown(SHADOW_TOUCH_MATRIX);
        return;
    default:
        return;
    }
}

/* The wire opcodes of the generated indexed enables (glEnablei, glDisablei):
   GL_BLEND and GL_SCISSOR_TEST are kept, and these change them. */
static void shadow_opcode(uint32_t opcode) {
    if (opcode == 1149u || opcode == 1125u) shadow_unknown(SHADOW_TOUCH_ENABLE);
}

static unsigned shadow_compared, shadow_differed;

static void shadow_compare(int differs, const char *what, unsigned name) {
    shadow_compared++;
    if (differs && shadow_differed++ < 20)
        fprintf(stderr, "gexgl: shadow %s 0x%x differs from the host's\n", what, name);
    if (shadow_compared % 5000u == 0)
        fprintf(stderr, "gexgl: shadow: %u answers compared, %u differed\n", shadow_compared, shadow_differed);
}

/* Under GEXGL_SHADOW_CHECK, when the context goes: how many answers were
   compared. */
static void shadow_report(void) {
    if (shadow_check() && !shadow_off())
        fprintf(stderr, "gexgl: shadow: %u answers compared, %u differed\n", shadow_compared, shadow_differed);
}

/* glGetFloatv/glGetDoublev: the matrix kept for GL_MODELVIEW_MATRIX or
   GL_PROJECTION_MATRIX, NULL when there is none to answer with. */
static const struct shadow_matrix *shadow_matrix_for(unsigned pname) {
    if (shadow_off() || shadow.in_begin) return NULL;
    const int s = pname == 0x0BA6u ? 0 : pname == 0x0BA7u ? 1 : -1;
    if (s < 0 || shadow.depth[s] < 0) return NULL;
    const struct shadow_matrix *t = &shadow.stack[s][shadow.depth[s]];
    return t->known ? t : NULL;
}

static int gexgl_shadow_get_float(unsigned pname, float *out) {
    const struct shadow_matrix *t = shadow_matrix_for(pname);
    if (!t || !out || shadow_check()) return 0;
    memcpy(out, t->m, sizeof(t->m));
    return 1;
}

static int gexgl_shadow_get_double(unsigned pname, double *out) {
    const struct shadow_matrix *t = shadow_matrix_for(pname);
    if (!t || !out || shadow_check()) return 0;
    for (int i = 0; i < 16; i++) out[i] = t->m[i];
    return 1;
}

/* The host's answer, kept (and, under GEXGL_SHADOW_CHECK, compared). */
static void gexgl_shadow_seed_float(unsigned pname, const float *got) {
    if (shadow_off() || shadow.in_begin || !got) return;
    const int s = pname == 0x0BA6u ? 0 : pname == 0x0BA7u ? 1 : -1;
    if (s < 0 || shadow.depth[s] < 0) return;
    struct shadow_matrix *t = &shadow.stack[s][shadow.depth[s]];
    if (shadow_check() && t->known) shadow_compare(memcmp(t->m, got, sizeof(t->m)) != 0, "matrix", pname);
    memcpy(t->m, got, sizeof(t->m));
    t->known = 1;
}

static void gexgl_shadow_seed_double(unsigned pname, const double *got) {
    if (!got) return;
    float f[16];
    for (int i = 0; i < 16; i++) f[i] = (float)got[i];
    gexgl_shadow_seed_float(pname, f);
}

static int gexgl_shadow_is_enabled(unsigned cap, unsigned char *out) {
    if (shadow_off() || shadow_check() || shadow.in_begin || shadow.listing) return 0;
    for (unsigned i = 0; i < shadow.n_enables; i++)
        if (shadow.enables[i].cap == cap) {
            *out = shadow.enables[i].on;
            return 1;
        }
    return 0;
}

static void shadow_set_enabled(unsigned cap, unsigned char on) {
    for (unsigned i = 0; i < shadow.n_enables; i++)
        if (shadow.enables[i].cap == cap) {
            shadow.enables[i].on = on;
            return;
        }
    if (shadow.n_enables < sizeof(shadow.enables) / sizeof(shadow.enables[0])) {
        shadow.enables[shadow.n_enables].cap = cap;
        shadow.enables[shadow.n_enables].on = on;
        shadow.n_enables++;
    }
}

static void gexgl_shadow_seed_enabled(unsigned cap, unsigned char on) {
    if (shadow_off() || shadow.in_begin || shadow.listing || !shadow_cap_kept(cap)) return;
    on = (unsigned char)(on != 0);
    if (shadow_check())
        for (unsigned i = 0; i < shadow.n_enables; i++)
            if (shadow.enables[i].cap == cap) shadow_compare(shadow.enables[i].on != on, "enable", cap);
    shadow_set_enabled(cap, on);
}

#ifdef SHADOW_POP_OPTIONS
#pragma GCC pop_options
#undef SHADOW_POP_OPTIONS
#endif
