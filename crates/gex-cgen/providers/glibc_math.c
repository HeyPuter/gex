/* PROVIDES libm.so.6 x86_64
 *
 * glibc's libm rounding functions (x86-64) as native wasm: floor, ceil,
 * trunc, round and their float
 * forms. Each result is exact, so it is glibc's bit for bit; a NaN comes
 * back quieted, as glibc's x + x makes it (wasm would be free to choose
 * the payload). Under gex's CPUID (no SSE4.1) glibc's IFUNCs pick the C
 * versions, bit manipulation (__floorf_c can be a visible share of a
 * program's time). No
 * exception flags are raised here; glibc's raise none for these either,
 * except invalid for a signalling NaN. sin, cos and sincos stay glibc's:
 * nothing short of glibc's own algorithm returns its bits. */

GEX_ELF_IFUNC

GEX_INLINE double arg_d(int ctx) { return gp_xmm_f64(ctx, 0); }
GEX_INLINE float arg_f(int ctx) { return gex_f32(GEX_C32(GEX_CTX_VN(0))); }
GEX_INLINE int ret_d(int ctx, uint64_t ret, double x, double r) {
    uint64_t b = gex_b64(x);
    if ((b & 0x7fffffffffffffffull) > 0x7ff0000000000000ull) {
        r = gex_f64(b | 0x0008000000000000ull);
    }
    gp_set_xmm_f64(ctx, 0, r);
    return gp64_return(ctx, ret);
}
GEX_INLINE int ret_f(int ctx, uint64_t ret, float x, float r) {
    uint32_t b = gex_b32(x);
    if ((b & 0x7fffffffu) > 0x7f800000u) {
        r = gex_f32(b | 0x00400000u);
    }
    GEX_C32(GEX_CTX_VN(0)) = gex_b32(r);
    return gp64_return(ctx, ret);
}

static GEX_PROVIDER(p_floor) {
    (void)depth, (void)need;
    double x = arg_d(ctx);
    return ret_d(ctx, ret, x, __builtin_floor(x));
}
static GEX_PROVIDER(p_floorf) {
    (void)depth, (void)need;
    float x = arg_f(ctx);
    return ret_f(ctx, ret, x, __builtin_floorf(x));
}
static GEX_PROVIDER(p_ceil) {
    (void)depth, (void)need;
    double x = arg_d(ctx);
    return ret_d(ctx, ret, x, __builtin_ceil(x));
}
static GEX_PROVIDER(p_ceilf) {
    (void)depth, (void)need;
    float x = arg_f(ctx);
    return ret_f(ctx, ret, x, __builtin_ceilf(x));
}
static GEX_PROVIDER(p_trunc) {
    (void)depth, (void)need;
    double x = arg_d(ctx);
    return ret_d(ctx, ret, x, __builtin_trunc(x));
}
static GEX_PROVIDER(p_truncf) {
    (void)depth, (void)need;
    float x = arg_f(ctx);
    return ret_f(ctx, ret, x, __builtin_truncf(x));
}
/* Half away from zero: x - trunc(x) is exact, so the comparison is too. */
static GEX_PROVIDER(p_round) {
    (void)depth, (void)need;
    double x = arg_d(ctx), r = __builtin_trunc(x);
    if (__builtin_fabs(x - r) >= 0.5) {
        r += __builtin_copysign(1.0, x);
    }
    return ret_d(ctx, ret, x, r);
}
static GEX_PROVIDER(p_roundf) {
    (void)depth, (void)need;
    float x = arg_f(ctx), r = __builtin_truncf(x);
    if (__builtin_fabsf(x - r) >= 0.5f) {
        r += __builtin_copysignf(1.0f, x);
    }
    return ret_f(ctx, ret, x, r);
}

GEX_PROVIDE(p_floor, "floor")
GEX_PROVIDE(p_floorf, "floorf")
GEX_PROVIDE(p_ceil, "ceil")
GEX_PROVIDE(p_ceilf, "ceilf")
GEX_PROVIDE(p_trunc, "trunc")
GEX_PROVIDE(p_truncf, "truncf")
GEX_PROVIDE(p_round, "round")
GEX_PROVIDE(p_roundf, "roundf")
