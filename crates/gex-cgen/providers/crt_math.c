/* PROVIDES msvcr120.dll,msvcrt.dll,ucrtbase.dll i386
 *
 * The CRT's exactly specified math:
 * floor, ceil, modf and sqrt, whose IEEE results do not depend on the
 * library, so native wasm matches any CRT to the bit (Microsoft's msvcr120
 * and Wine's alike). The x87-ABI exports (cdecl, a double on the
 * stack, the result pushed on the x87 stack) and msvcr120's
 * `_libm_sse2_sqrt_precise` (xmm0 in and out).
 *
 * The x87 sqrt is fsqrt at the caller's precision control, rounded to
 * nearest, then (usually) to a double: see p_sqrt.
 * _libm_sse2_sqrt_precise rounds to nearest in Microsoft's build and by
 * MXCSR in Wine's (directed modes by an exact correction). */

static double quiet(double x) {
    return gex_f64(gex_b64(x) | 0x0008000000000000ull);
}
static int isnan_(double x) {
    return (gex_b64(x) & 0x7fffffffffffffffull) > 0x7ff0000000000000ull;
}

/* x * x as an unevaluated sum hi + lo (Dekker, no FMA needed). */
GEX_INLINE void sq_exact(double x, double *hi, double *lo) {
    double t = x * 134217729.0, xh = t - (t - x), xl = x - xh;
    *hi = x * x;
    *lo = ((xh * xh - *hi) + 2.0 * xh * xl) + xl * xl;
}
static double next_up(double x) {
    uint64_t b = gex_b64(x);
    return gex_f64(x == 0.0 ? 1ull : (b >> 63) ? b - 1 : b + 1);
}
static double next_down(double x) {
    uint64_t b = gex_b64(x);
    return gex_f64(x == 0.0 ? 0x8000000000000001ull : (b >> 63) ? b + 1 : b - 1);
}
/* sqrt(x) for x > 0 finite, rounded by `rc` (0 nearest, 1 down, 2 up, 3 to
 * zero): the nearest result, moved one ulp when the exact root lies on the
 * other side of it. */
static double sqrt_rc(double x, uint32_t rc) {
    /* Keep r*r and its low part in range: scale by 2^(+-108), exactly. */
    if (rc && x < 0x1p-900) {
        return sqrt_rc(x * 0x1p108, rc) * 0x1p-54;
    }
    if (rc && x > 0x1p900) {
        return sqrt_rc(x * 0x1p-108, rc) * 0x1p54;
    }
    double r = __builtin_sqrt(x);
    if (rc == 0) {
        return r;
    }
    double hi, lo;
    sq_exact(r, &hi, &lo);
    /* sign of r*r - x, exactly: hi - x is exact when they are this close */
    double d = (hi - x) + lo;
    if (rc == 2) {
        return d < 0 ? next_up(r) : r;
    }
    return d > 0 ? next_down(r) : r; /* down, and toward zero (r > 0) */
}
/* The error cases, as each library has them (as a test run
 * against Microsoft's msvcr120 and Wine's builds sees them):
 * - a NaN argument comes back quieted with its payload; Microsoft's floor,
 *   ceil, modf and sqrt (both entries) set errno = EDOM for it, Wine's x87
 *   sqrt does too, its floor, ceil and modf do not, and its
 *   _libm_sse2_sqrt_precise returns the NaN unchanged (not even quieted)
 *   without errno;
 * - sqrt of a negative number (not -0) is the default NaN with EDOM in all.
 * Only these slow paths look at which library is loaded (gp_wine_builtin). */
static int wine(uint32_t self) {
    uint32_t b = gp_module_base(self);
    return b && gp_wine_builtin(b);
}
/* floor/ceil/modf of a NaN: quieted; EDOM unless Wine. */
static double nan_rounding(int ctx, int32_t depth, uint32_t self, double x) {
    if (!wine(self)) {
        gp_set_errno(ctx, depth, self, GP_EDOM);
    }
    return quiet(x);
}

static GEX_PROVIDER(p_floor) {
    (void)need;
    uint32_t self = (uint32_t)GP_SELF_PC();
    double x = gp_argf64(ctx, 0);
    gp_push_st0(ctx, isnan_(x) ? nan_rounding(ctx, depth, self, x) : __builtin_floor(x));
    return gp_return(ctx, ret, 0);
}
static GEX_PROVIDER(p_ceil) {
    (void)need;
    uint32_t self = (uint32_t)GP_SELF_PC();
    double x = gp_argf64(ctx, 0);
    gp_push_st0(ctx, isnan_(x) ? nan_rounding(ctx, depth, self, x) : __builtin_ceil(x));
    return gp_return(ctx, ret, 0);
}
/* modf(x, double *ip): the fraction, signed as x; the integer part through ip. */
static GEX_PROVIDER(p_modf) {
    (void)need;
    uint32_t self = (uint32_t)GP_SELF_PC();
    double x = gp_argf64(ctx, 0), ip, fr;
    uint32_t p = gp_arg(ctx, 2);
    if (isnan_(x)) {
        ip = fr = nan_rounding(ctx, depth, self, x);
    } else if (__builtin_fabs(x) == __builtin_inf()) {
        ip = x;
        fr = __builtin_copysign(0.0, x);
    } else {
        ip = __builtin_trunc(x);
        fr = __builtin_copysign(x - ip, x);
    }
    *(gex_u64 *)GEX_P(p) = gex_b64(ip);
    gp_push_st0(ctx, fr);
    return gp_return(ctx, ret, 0);
}
/* sqrt's domain errors: a NaN quieted, a negative the default NaN; EDOM. */
static double sqrt_error(int ctx, int32_t depth, uint32_t self, double x) {
    if (isnan_(x)) {
        gp_set_errno(ctx, depth, self, GP_EDOM);
        return quiet(x);
    }
    gp_set_errno(ctx, depth, self, GP_EDOM);
    return gex_f64(0xfff8000000000000ull);
}
static int sqrt_domain(double x) {
    return isnan_(x) || x < 0;
}
/* ---- fsqrt, exactly: the x87 sqrt entries (Microsoft's and Wine's) run
 * fsqrt at the caller's precision control, rounding to nearest: a 24-, 53-
 * or 64-bit significand in ST0, not necessarily a double. */

/* a * b as hi:lo, from 32-bit halves (no 128-bit libcalls here). */
GEX_INLINE void mul64(uint64_t a, uint64_t b, uint64_t *hi, uint64_t *lo) {
    uint64_t al = (uint32_t)a, ah = a >> 32, bl = (uint32_t)b, bh = b >> 32;
    uint64_t ll = al * bl, lh = al * bh, hl = ah * bl, hh = ah * bh;
    uint64_t mid = (ll >> 32) + (uint32_t)lh + (uint32_t)hl;
    *lo = (mid << 32) | (uint32_t)ll;
    *hi = hh + (lh >> 32) + (hl >> 32) + (mid >> 32);
}
/* r*r <= nh:nl */
GEX_INLINE int sq_le(uint64_t r, uint64_t nh, uint64_t nl) {
    uint64_t h, l;
    mul64(r, r, &h, &l);
    return h < nh || (h == nh && l <= nl);
}
/* fsqrt of a positive finite double at `bits` (24, 53 or 64) of precision,
 * round to nearest: the extended result, and *sw's precision (bit 5) and
 * round-up C1 (bit 9) flags. */
GEX_INLINE void fsqrt_pc(double x, uint32_t bits, uint64_t *sig, uint32_t *se, uint32_t *sw) {
    uint64_t b = gex_b64(x), m = b & 0xfffffffffffffull;
    int32_t e = (int32_t)((b >> 52) & 0x7ff), E;
    if (e) {
        m |= 1ull << 52;
        E = e - 1075;
    } else {
        int z = __builtin_clzll(m) - 11;
        m <<= z;
        E = -1074 - z;
    }
    /* N = m << sh, sh 74 or 75 with E - sh even: sqrt(N) in [2^63, 2^64). */
    uint32_t sh = ((E - 74) & 1) ? 75 : 74;
    uint64_t nh = m << (sh - 64), nl = 0;
    /* R = floor(sqrt(N)): a double estimate, one Newton step, then +-1. */
    double est = __builtin_sqrt((double)m * 18446744073709551616.0 * (double)(1ull << (sh - 64)));
    uint64_t r = est >= 18446744073709551615.0 ? 0xffffffffffffffffull : (uint64_t)est;
    while (!sq_le(r, nh, nl)) {
        --r;
    }
    while (r != 0xffffffffffffffffull && sq_le(r + 1, nh, nl)) {
        ++r;
    }
    uint64_t h, l;
    mul64(r, r, &h, &l);
    /* remainder N - R^2 (< 2^65): zero, and whether it exceeds R */
    uint64_t rl = nl - l, rh = nh - h - (nl < l);
    int sticky = (rl | rh) != 0, over = rh != 0 || rl > r;
    int32_t ex = 16383 + 63 + (E - (int32_t)sh) / 2;
    int up, inexact;
    if (bits >= 64) {
        up = over;
        inexact = sticky;
    } else {
        uint32_t drop = 64 - bits;
        uint64_t low = r & ((1ull << drop) - 1), half = 1ull << (drop - 1);
        up = low > half || (low == half && (sticky || ((r >> drop) & 1)));
        inexact = low != 0 || sticky;
        r &= ~((1ull << drop) - 1);
        if (up) {
            uint64_t old = r;
            r += 1ull << drop;
            if (r < old) {
                r = 1ull << 63;
                ++ex;
            }
        }
    }
    if (bits >= 64 && up) {
        if (++r == 0) {
            r = 1ull << 63;
            ++ex;
        }
    }
    *sig = r;
    *se = (uint32_t)ex;
    *sw = (inexact ? 0x20u : 0) | (up ? 0x200u : 0);
}

/* The x87 sqrt's value for a positive finite x under control word `cw`,
 * as both libraries compute it (msvcr120's sqrt at 0x10036e0b and its exit
 * at 0x1004639e; Wine's agrees in every tested case): a caller control word
 * other than 0x27f is replaced by its precision control with every
 * exception masked and rounding to nearest; then fsqrt; then, unless the
 * caller's word was 0x27f, an inexact result under a masked precision
 * exception goes through the exception helper, which stores it as a double
 * (nearest) and loads it back. The extended result, and the status flags. */
GEX_INLINE void x87_sqrt(uint32_t cw, double x, uint64_t *sigp, uint32_t *sep, uint32_t *swp) {
    uint32_t pc = (cw >> 8) & 3, se, sw; /* PC 00 = 24 bits, 10 = 53, 11 = 64 */
    uint64_t sig;
    fsqrt_pc(x, cw == 0x27f ? 53 : pc == 0 ? 24 : pc == 3 ? 64 : 53, &sig, &se, &sw);
    if (cw != 0x27f && (cw & 0x20) && (sw & 0x20)) {
        uint64_t low = sig & 0x7ff; /* to a double, nearest even */
        sig &= ~0x7ffull;
        if (low > 0x400 || (low == 0x400 && (sig & 0x800))) {
            sig += 0x800;
            if (!sig) {
                sig = 1ull << 63;
                ++se;
            }
        }
    }
    *sigp = sig, *sep = se, *swp = sw;
}
/* A normal extended value as a double, rounded by `rc` (fst m64). */
static double ext_to_double(uint64_t sig, uint32_t se, uint32_t rc) {
    uint64_t m = sig >> 11, low = sig & 0x7ff;
    uint32_t sign = se >> 15;
    int32_t e = (int32_t)(se & 0x7fff) - 16383 + 1023;
    int up = rc == 0 ? (low > 0x400 || (low == 0x400 && (m & 1))) : rc == 3 ? 0 : (rc == 1) == (sign != 0) ? low != 0 : 0;
    m += (uint64_t)up;
    if (m >> 53) {
        m >>= 1;
        ++e;
    }
    return gex_f64(((uint64_t)sign << 63) | ((uint64_t)e << 52) | (m & 0xfffffffffffffull));
}

static GEX_PROVIDER(p_sqrt) {
    (void)need;
    uint32_t self = (uint32_t)GP_SELF_PC();
    double x = gp_argf64(ctx, 0);
    if (sqrt_domain(x)) {
        gp_push_st0(ctx, sqrt_error(ctx, depth, self, x));
    } else if (x == 0.0 || x == __builtin_inf()) {
        gp_push_st0(ctx, x);
    } else {
        uint64_t sig;
        uint32_t se, sw;
        x87_sqrt(GEX_C32(GEX_CTX_X87_CW) & 0xffff, x, &sig, &se, &sw);
        gp_push_st0_raw(ctx, sig, se);
        GEX_C32(GEX_CTX_X87_SW) = (GEX_C32(GEX_CTX_X87_SW) & ~0x200u) | sw;
    }
    return gp_return(ctx, ret, 0);
}
/* _libm_sse2_sqrt_precise. Both libraries take their SSE2 path only in
 * the default floating-point state (MXCSR's masks and rounding as at
 * start-up, the x87's exceptions masked): sqrtsd, and for a NaN Wine's
 * returns it unchanged without errno. Otherwise Microsoft's runs its x87
 * routine (x87_sqrt, then stored to xmm0 under the x87 rounding control) and
 * Wine's rounds by MXCSR; both then quiet a NaN and set EDOM. */
static GEX_PROVIDER(p_sse2_sqrt) {
    (void)need;
    uint32_t self = (uint32_t)GP_SELF_PC();
    double x = gp_xmm_f64(ctx, 0), r;
    uint32_t mx = GEX_C32(GEX_CTX_MXCSR), cw = GEX_C32(GEX_CTX_X87_CW) & 0xffff;
    int dflt = (mx & 0x7f80) == 0x1f80 && (cw & 0x7f) == 0x7f;
    if (isnan_(x)) {
        r = dflt && wine(self) ? x : sqrt_error(ctx, depth, self, x);
    } else if (x < 0) {
        r = sqrt_error(ctx, depth, self, x);
    } else if (dflt || x == 0.0 || x == __builtin_inf()) {
        r = __builtin_sqrt(x);
    } else if (wine(self)) {
        r = sqrt_rc(x, (mx >> 13) & 3);
    } else {
        uint64_t sig;
        uint32_t se, sw;
        x87_sqrt(cw, x, &sig, &se, &sw);
        r = ext_to_double(sig, se, (cw >> 10) & 3);
    }
    gp_set_xmm_f64(ctx, 0, r);
    return gp_return(ctx, ret, 0);
}

GEX_PROVIDE(p_floor, "floor")
GEX_PROVIDE(p_ceil, "ceil")
GEX_PROVIDE(p_modf, "modf")
GEX_PROVIDE(p_sqrt, "sqrt")
GEX_PROVIDE(p_sse2_sqrt, "_libm_sse2_sqrt_precise")
