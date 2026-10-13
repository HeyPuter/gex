/* PROVIDES msvcr120.dll i386 wineonly lifted
 *
 * Wine's msvcr120 trigonometry (docs/merge-plan.md §9.9.2 step 3): sin, cos,
 * tan, atan, atan2 and rint, with their x87-ABI (_CI*) and SSE2-ABI
 * (__libm_sse2_*, _libm_sse2_*_precise) entries. Wine builds these from
 * its copy of musl (libs/musl/src/math) as i386 code, so they run on the
 * x87, which gex lifts exactly (80-bit, the control word's precision and
 * rounding: runtime/x87.h). Native wasm doubles match that bit for bit
 * when the control word is the default one -- 53-bit precision, rounding
 * to nearest, every exception masked -- and every value stays a normal
 * double or zero: musl's arithmetic, done in doubles in the same order,
 * then rounds exactly as the x87 does.
 *
 * Everything else runs Wine's own lifted body (`lifted`, GP_LIFTED):
 * another control word (the results then depend on where Wine's compiled
 * code spills to memory), a NaN or infinite argument (Wine's math_error
 * calls the program's _matherr), arguments past 2^20 pi/2 (musl's
 * __rem_pio2_large), an atan2 whose quotient could be subnormal (the x87
 * rounds it twice), and an x87-ABI call with an x87 register that is not
 * an exact double (Wine's wrapper stores every register as a double).
 * Microsoft's msvcr120 has other algorithms: Wine's build only
 * (`wineonly`). tests/providers crtmath checks both paths. */

/* ---- musl, in doubles (no tables: a provider module has no data) ---- */

GEX_INLINE uint32_t hiword(double x) { return (uint32_t)(gex_b64(x) >> 32); }
GEX_INLINE double with_low0(double x) { return gex_f64(gex_b64(x) & 0xffffffff00000000ull); }

static double k_sin(double x, double y, int iy) {
    const double S1 = -1.66666666666666324348e-01, S2 = 8.33333333332248946124e-03, S3 = -1.98412698298579493134e-04,
                 S4 = 2.75573137070700676789e-06, S5 = -2.50507602534068634195e-08, S6 = 1.58969099521155010221e-10;
    double z, r, v, w;
    z = x * x;
    w = z * z;
    r = S2 + z * (S3 + z * S4) + z * w * (S5 + z * S6);
    v = z * x;
    if (iy == 0) {
        return x + v * (S1 + z * r);
    }
    return x - ((z * (0.5 * y - v * r) - y) - v * S1);
}

static double k_cos(double x, double y) {
    const double C1 = 4.16666666666666019037e-02, C2 = -1.38888888888741095749e-03, C3 = 2.48015872894767294178e-05,
                 C4 = -2.75573143513906633035e-07, C5 = 2.08757232129817482790e-09, C6 = -1.13596475577881948265e-11;
    double hz, z, r, w;
    z = x * x;
    w = z * z;
    r = z * (C1 + z * (C2 + z * C3)) + w * w * (C4 + z * (C5 + z * C6));
    hz = 0.5 * z;
    w = 1.0 - hz;
    return w + (((1.0 - w) - hz) + (z * r - x * y));
}

static double k_tan(double x, double y, int odd) {
    const double T0 = 3.33333333333334091986e-01, T1 = 1.33333333333201242699e-01, T2 = 5.39682539762260521377e-02,
                 T3 = 2.18694882948595424599e-02, T4 = 8.86323982359930005737e-03, T5 = 3.59207910759131235356e-03,
                 T6 = 1.45620945432529025516e-03, T7 = 5.88041240820264096874e-04, T8 = 2.46463134818469906812e-04,
                 T9 = 7.81794442939557092300e-05, T10 = 7.14072491382608190305e-05, T11 = -1.85586374855275456654e-05,
                 T12 = 2.59073051863633712884e-05, pio4 = 7.85398163397448278999e-01, pio4lo = 3.06161699786838301793e-17;
    double z, r, v, w, s, a, w0, a0;
    uint32_t hx = hiword(x);
    int big = (hx & 0x7fffffff) >= 0x3FE59428, sign = 0;
    if (big) {
        sign = hx >> 31;
        if (sign) {
            x = -x;
            y = -y;
        }
        x = (pio4 - x) + (pio4lo - y);
        y = 0.0;
    }
    z = x * x;
    w = z * z;
    r = T1 + w * (T3 + w * (T5 + w * (T7 + w * (T9 + w * T11))));
    v = z * (T2 + w * (T4 + w * (T6 + w * (T8 + w * (T10 + w * T12)))));
    s = z * x;
    r = y + z * (s * (r + v) + y) + s * T0;
    w = x + r;
    if (big) {
        s = 1 - 2 * odd;
        v = s - 2.0 * (x + (r - w * w / (w + s)));
        return sign ? -v : v;
    }
    if (!odd) {
        return w;
    }
    w0 = with_low0(w);
    v = r - (w0 - x);
    a0 = a = -1.0 / w;
    a0 = with_low0(a0);
    return a0 + a * (1.0 + a0 * w0 + a0 * v);
}

/* musl's __rem_pio2 for |x| < 2^20 pi/2 (`ix` < 0x413921fb): the caller
 * leaves larger arguments to the lifted body. Wine's copy rounds with rint,
 * at the default control word to nearest, ties to even. */
GEX_INLINE int rem_pio2(double x, double *y0, double *y1) {
    const double pio4 = 0x1.921fb54442d18p-1, invpio2 = 6.36619772367581382433e-01, pio2_1 = 1.57079632673412561417e+00,
                 pio2_1t = 6.07710050650619224932e-11, pio2_2 = 6.07710050630396597660e-11, pio2_2t = 2.02226624879595063154e-21,
                 pio2_3 = 2.02226624871116645580e-21, pio2_3t = 8.47842766036889956997e-32;
    double z, w, t, r, fn;
    uint32_t ix = hiword(x) & 0x7fffffff;
    int sign = (int)(gex_b64(x) >> 63), n, ex, ey;
    if (ix <= 0x400f6a7a) {
        if ((ix & 0xfffff) == 0x921fb) {
            goto medium;
        }
        if (ix <= 0x4002d97c) {
            if (!sign) {
                z = x - pio2_1;
                *y0 = z - pio2_1t;
                *y1 = (z - *y0) - pio2_1t;
                return 1;
            }
            z = x + pio2_1;
            *y0 = z + pio2_1t;
            *y1 = (z - *y0) + pio2_1t;
            return -1;
        }
        if (!sign) {
            z = x - 2 * pio2_1;
            *y0 = z - 2 * pio2_1t;
            *y1 = (z - *y0) - 2 * pio2_1t;
            return 2;
        }
        z = x + 2 * pio2_1;
        *y0 = z + 2 * pio2_1t;
        *y1 = (z - *y0) + 2 * pio2_1t;
        return -2;
    }
    if (ix <= 0x401c463b) {
        if (ix <= 0x4015fdbc) {
            if (ix == 0x4012d97c) {
                goto medium;
            }
            if (!sign) {
                z = x - 3 * pio2_1;
                *y0 = z - 3 * pio2_1t;
                *y1 = (z - *y0) - 3 * pio2_1t;
                return 3;
            }
            z = x + 3 * pio2_1;
            *y0 = z + 3 * pio2_1t;
            *y1 = (z - *y0) + 3 * pio2_1t;
            return -3;
        }
        if (ix == 0x401921fb) {
            goto medium;
        }
        if (!sign) {
            z = x - 4 * pio2_1;
            *y0 = z - 4 * pio2_1t;
            *y1 = (z - *y0) - 4 * pio2_1t;
            return 4;
        }
        z = x + 4 * pio2_1;
        *y0 = z + 4 * pio2_1t;
        *y1 = (z - *y0) + 4 * pio2_1t;
        return -4;
    }
medium:
    fn = __builtin_rint(x * invpio2);
    n = (int32_t)fn;
    r = x - fn * pio2_1;
    w = fn * pio2_1t;
    if (r - w < -pio4) {
        n--;
        fn--;
        r = x - fn * pio2_1;
        w = fn * pio2_1t;
    } else if (r - w > pio4) {
        n++;
        fn++;
        r = x - fn * pio2_1;
        w = fn * pio2_1t;
    }
    *y0 = r - w;
    ey = (int)(hiword(*y0) >> 20) & 0x7ff;
    ex = (int)(ix >> 20);
    if (ex - ey > 16) {
        t = r;
        w = fn * pio2_2;
        r = t - w;
        w = fn * pio2_2t - ((t - r) - w);
        *y0 = r - w;
        ey = (int)(hiword(*y0) >> 20) & 0x7ff;
        if (ex - ey > 49) {
            t = r;
            w = fn * pio2_3;
            r = t - w;
            w = fn * pio2_3t - ((t - r) - w);
            *y0 = r - w;
        }
    }
    *y1 = (r - *y0) - w;
    return n;
}

/* The arguments taken here: finite, |x| < 2^20 pi/2. */
GEX_INLINE int trig_arg(double x) { return (hiword(x) & 0x7fffffff) < 0x413921fb; }

static double w_sin(double x) {
    uint32_t ix = hiword(x) & 0x7fffffff;
    double y0, y1;
    if (ix <= 0x3fe921fb) {
        return ix < 0x3e500000 ? x : k_sin(x, 0.0, 0);
    }
    switch (rem_pio2(x, &y0, &y1) & 3) {
    case 0: return k_sin(y0, y1, 1);
    case 1: return k_cos(y0, y1);
    case 2: return -k_sin(y0, y1, 1);
    default: return -k_cos(y0, y1);
    }
}
static double w_cos(double x) {
    uint32_t ix = hiword(x) & 0x7fffffff;
    double y0, y1;
    if (ix <= 0x3fe921fb) {
        return ix < 0x3e46a09e ? 1.0 : k_cos(x, 0);
    }
    switch (rem_pio2(x, &y0, &y1) & 3) {
    case 0: return k_cos(y0, y1);
    case 1: return -k_sin(y0, y1, 1);
    case 2: return -k_cos(y0, y1);
    default: return k_sin(y0, y1, 1);
    }
}
static double w_tan(double x) {
    uint32_t ix = hiword(x) & 0x7fffffff;
    double y0, y1;
    if (ix <= 0x3fe921fb) {
        return ix < 0x3e400000 ? x : k_tan(x, 0.0, 0);
    }
    int n = rem_pio2(x, &y0, &y1);
    return k_tan(y0, y1, n & 1);
}

/* atan for a finite x. */
static double w_atan(double x) {
    const double aT0 = 3.33333333333329318027e-01, aT1 = -1.99999999998764832476e-01, aT2 = 1.42857142725034663711e-01,
                 aT3 = -1.11111104054623557880e-01, aT4 = 9.09088713343650656196e-02, aT5 = -7.69187620504482999495e-02,
                 aT6 = 6.66107313738753120669e-02, aT7 = -5.83357013379057348645e-02, aT8 = 4.97687799461593236017e-02,
                 aT9 = -3.65315727442169155270e-02, aT10 = 1.62858201153657823623e-02;
    double w, s1, s2, z, hi, lo;
    uint32_t ix = hiword(x), sign = ix >> 31;
    int id;
    ix &= 0x7fffffff;
    if (ix >= 0x44100000) {
        z = 1.57079632679489655800e+00 + 0x1p-120f;
        return sign ? -z : z;
    }
    if (ix < 0x3fdc0000) {
        if (ix < 0x3e400000) {
            return x;
        }
        id = -1;
    } else {
        x = __builtin_fabs(x);
        if (ix < 0x3ff30000) {
            if (ix < 0x3fe60000) {
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if (ix < 0x40038000) {
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            id = 3;
            x = -1.0 / x;
        }
    }
    z = x * x;
    w = z * z;
    s1 = z * (aT0 + w * (aT2 + w * (aT4 + w * (aT6 + w * (aT8 + w * aT10)))));
    s2 = w * (aT1 + w * (aT3 + w * (aT5 + w * (aT7 + w * aT9))));
    if (id < 0) {
        return x - x * (s1 + s2);
    }
    hi = id == 0 ? 4.63647609000806093515e-01 : id == 1 ? 7.85398163397448278999e-01 : id == 2 ? 9.82793723247329054082e-01 : 1.57079632679489655800e+00;
    lo = id == 0 ? 2.26987774529616870924e-17 : id == 1 ? 3.06161699786838301793e-17 : id == 2 ? 1.39033110312309984516e-17 : 6.12323399573676603587e-17;
    z = hi - (x * (s1 + s2) - lo - x);
    return sign ? -z : z;
}

/* Whether atan2(y, x) of finite y and x is taken here: not when |y/x|
 * could be subnormal (below 2^-1000, short of musl's own 2^-64 cut for a
 * negative x), which the x87 rounds twice. */
GEX_INLINE int atan2_args(double y, double x) {
    uint32_t ix = hiword(x) & 0x7fffffff, iy = hiword(y) & 0x7fffffff;
    int yzero = (gex_b64(y) << 1) == 0;
    return yzero || (gex_b64(x) >> 63) || iy + (1000u << 20) >= ix;
}
static double w_atan2(double y, double x) {
    const double pi = 3.1415926535897931160E+00, pi_lo = 1.2246467991473531772E-16;
    double z;
    uint32_t m, lx, ly, ix, iy;
    uint64_t bx = gex_b64(x), by = gex_b64(y);
    ix = (uint32_t)(bx >> 32), lx = (uint32_t)bx;
    iy = (uint32_t)(by >> 32), ly = (uint32_t)by;
    if (((ix - 0x3ff00000) | lx) == 0) {
        return w_atan(y);
    }
    m = ((iy >> 31) & 1) | ((ix >> 30) & 2);
    ix &= 0x7fffffff;
    iy &= 0x7fffffff;
    if ((iy | ly) == 0) {
        switch (m) {
        case 0:
        case 1: return y;
        case 2: return pi;
        default: return -pi;
        }
    }
    if ((ix | lx) == 0) {
        return m & 1 ? -pi / 2 : pi / 2;
    }
    if (ix + (64 << 20) < iy) {
        return m & 1 ? -pi / 2 : pi / 2;
    }
    if ((m & 2) && iy + (64 << 20) < ix) {
        z = 0;
    } else {
        z = w_atan(__builtin_fabs(y / x));
    }
    switch (m) {
    case 0: return z;
    case 1: return -z;
    case 2: return pi - (z - pi_lo);
    default: return (z - pi_lo) - pi;
    }
}

/* ---- the guest's side ---- */

/* The x87 control word gex lifts exactly as doubles: 53-bit precision,
 * nearest, all exceptions masked (Windows' and Wine's default, 0x27f). */
GEX_INLINE int std_cw(int ctx) { return (GEX_C32(GEX_CTX_X87_CW) & 0xf3fu) == 0x23fu; }
GEX_INLINE int finite_(double x) { return (gex_b64(x) & 0x7ff0000000000000ull) != 0x7ff0000000000000ull; }

/* Whether Wine's x87-ABI wrapper would see exactly the guest's registers:
 * the `n` arguments present and every occupied register an exact double
 * (the wrapper stores them all as doubles and loads them back). */
static int x87_doubles(int ctx, uint32_t n) {
    uint32_t top = GEX_C32(GEX_CTX_X87_TOP) & 7u, tags = GEX_C32(GEX_CTX_X87_TAGS);
    for (uint32_t i = 0; i < 8; i++) {
        uint32_t p = (top + i) & 7u;
        if (!((tags >> p) & 1u)) {
            return i >= n;
        }
        uint64_t sig = GEX_C64(GEX_CTX_X87 + 16u * p);
        uint32_t e = GEX_C16(GEX_CTX_X87 + 16u * p + 8u) & 0x7fffu;
        if (!(e == 0 && sig == 0) && (e < 16383u - 1022u || e > 16383u + 1023u || !(sig >> 63) || (sig & 0x7ff))) {
            return 0;
        }
    }
    return 1;
}
/* ST(i) as a double (x87_doubles said it is one). */
GEX_INLINE double x87_st(int ctx, uint32_t i) {
    uint32_t p = (GEX_C32(GEX_CTX_X87_TOP) + i) & 7u;
    uint64_t sig = GEX_C64(GEX_CTX_X87 + 16u * p);
    uint32_t se = GEX_C16(GEX_CTX_X87 + 16u * p + 8u), e = se & 0x7fffu;
    uint64_t sign = (uint64_t)(se >> 15) << 63;
    if (e == 0) {
        return gex_f64(sign);
    }
    return gex_f64(sign | ((uint64_t)(e - 16383u + 1023u) << 52) | ((sig >> 11) & 0xfffffffffffffull));
}
/* Pop `n` registers. */
GEX_INLINE void x87_pop(int ctx, uint32_t n) {
    uint32_t top = GEX_C32(GEX_CTX_X87_TOP) & 7u;
    for (uint32_t i = 0; i < n; i++) {
        GEX_C32(GEX_CTX_X87_TAGS) &= ~(1u << top);
        top = (top + 1u) & 7u;
    }
    GEX_C32(GEX_CTX_X87_TOP) = top;
}
/* movq xmm0, m64: the double, the high half zeroed. */
GEX_INLINE void set_xmm0_movq(int ctx, double v) {
    GEX_C64(GEX_CTX_VN(0)) = gex_b64(v);
    GEX_C64(GEX_CTX_VN(0) + 8u) = 0;
}

#define CDECL1(name, f, ok)                                    \
    static GEX_PROVIDER(name) {                                \
        uint32_t self = (uint32_t)GP_SELF_PC();                \
        double x = gp_argf64(ctx, 0);                          \
        if (!std_cw(ctx) || !finite_(x) || !(ok)) {            \
            GP_LIFTED(self);                                   \
        }                                                      \
        gp_push_st0(ctx, f(x));                                \
        return gp_return(ctx, ret, 0);                         \
    }
#define SSE1(name, f, ok)                                      \
    static GEX_PROVIDER(name) {                                \
        uint32_t self = (uint32_t)GP_SELF_PC();                \
        double x = gp_xmm_f64(ctx, 0);                         \
        if (!std_cw(ctx) || !finite_(x) || !(ok)) {            \
            GP_LIFTED(self);                                   \
        }                                                      \
        set_xmm0_movq(ctx, f(x));                              \
        return gp_return(ctx, ret, 0);                         \
    }
/* An x87-ABI entry: the argument in ST0, popped; the result pushed. */
#define CI1(name, f, ok)                                       \
    static GEX_PROVIDER(name) {                                \
        uint32_t self = (uint32_t)GP_SELF_PC();                \
        if (!std_cw(ctx) || !x87_doubles(ctx, 1)) {            \
            GP_LIFTED(self);                                   \
        }                                                      \
        double x = x87_st(ctx, 0);                             \
        if (!finite_(x) || !(ok)) {                            \
            GP_LIFTED(self);                                   \
        }                                                      \
        x87_pop(ctx, 1);                                       \
        gp_push_st0(ctx, f(x));                                \
        return gp_return(ctx, ret, 0);                         \
    }

CDECL1(p_sin, w_sin, trig_arg(x))
CDECL1(p_cos, w_cos, trig_arg(x))
CDECL1(p_tan, w_tan, trig_arg(x))
CDECL1(p_atan, w_atan, 1)
CDECL1(p_rint, __builtin_rint, 1)
SSE1(p_sse2_sin, w_sin, trig_arg(x))
SSE1(p_sse2_cos, w_cos, trig_arg(x))
SSE1(p_sse2_tan, w_tan, trig_arg(x))
SSE1(p_sse2_atan, w_atan, 1)
CI1(p_ci_sin, w_sin, trig_arg(x))
CI1(p_ci_cos, w_cos, trig_arg(x))
CI1(p_ci_tan, w_tan, trig_arg(x))
CI1(p_ci_atan, w_atan, 1)

static GEX_PROVIDER(p_atan2) {
    uint32_t self = (uint32_t)GP_SELF_PC();
    double y = gp_argf64(ctx, 0), x = gp_argf64(ctx, 8);
    if (!std_cw(ctx) || !finite_(x) || !finite_(y) || !atan2_args(y, x)) {
        GP_LIFTED(self);
    }
    gp_push_st0(ctx, w_atan2(y, x));
    return gp_return(ctx, ret, 0);
}
static GEX_PROVIDER(p_sse2_atan2) {
    uint32_t self = (uint32_t)GP_SELF_PC();
    double y = gp_xmm_f64(ctx, 0), x = gp_xmm_f64(ctx, 1);
    if (!std_cw(ctx) || !finite_(x) || !finite_(y) || !atan2_args(y, x)) {
        GP_LIFTED(self);
    }
    set_xmm0_movq(ctx, w_atan2(y, x));
    return gp_return(ctx, ret, 0);
}
/* _CIatan2: atan2(ST1, ST0). */
static GEX_PROVIDER(p_ci_atan2) {
    uint32_t self = (uint32_t)GP_SELF_PC();
    if (!std_cw(ctx) || !x87_doubles(ctx, 2)) {
        GP_LIFTED(self);
    }
    double y = x87_st(ctx, 1), x = x87_st(ctx, 0);
    if (!finite_(x) || !finite_(y) || !atan2_args(y, x)) {
        GP_LIFTED(self);
    }
    x87_pop(ctx, 2);
    gp_push_st0(ctx, w_atan2(y, x));
    return gp_return(ctx, ret, 0);
}

GEX_PROVIDE(p_sin, "sin")
GEX_PROVIDE(p_cos, "cos")
GEX_PROVIDE(p_tan, "tan")
GEX_PROVIDE(p_atan, "atan")
GEX_PROVIDE(p_atan2, "atan2")
GEX_PROVIDE(p_rint, "rint")
GEX_PROVIDE(p_sse2_sin, "__libm_sse2_sin")
GEX_PROVIDE(p_sse2_cos, "__libm_sse2_cos")
GEX_PROVIDE(p_sse2_tan, "__libm_sse2_tan")
GEX_PROVIDE(p_sse2_atan, "__libm_sse2_atan")
GEX_PROVIDE(p_sse2_atan2, "__libm_sse2_atan2")
GEX_PROVIDE(p_ci_sin, "_CIsin")
GEX_PROVIDE(p_ci_cos, "_CIcos")
GEX_PROVIDE(p_ci_tan, "_CItan")
GEX_PROVIDE(p_ci_atan, "_CIatan")
GEX_PROVIDE(p_ci_atan2, "_CIatan2")
