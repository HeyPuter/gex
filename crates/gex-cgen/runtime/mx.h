/* The function tier's slow path of Expr::Mx (an SSE operation under a
 * MXCSR with a rounding control other than to nearest, FTZ or DAZ): the
 * gex.mx import, which computes it exactly (gex_f80::sse::op), as tier-1's
 * `emit_mx` calls it -- once for a scalar, once per lane, lanes past `n`
 * zero. The fast path is the operation's own C: kept when none of those
 * bits is set, and under FTZ or DAZ alone when no float operand nor the
 * result is a denormal or the smallest normal (the gex_mx_tiny tests), as
 * neither flush then has anything to do.
 *
 * Included from gex_abi.h, after the imports. */

/* A denormal or the smallest normal (|x| - 1 below the smallest normal). */
GEX_INLINE int gex_mx_tiny32(uint32_t x) { return ((x & 0x7fffffffu) - 1) >> 23 == 0; }
GEX_INLINE int gex_mx_tiny64(uint64_t x) { return ((x & 0x7fffffffffffffffull) - 1) >> 52 == 0; }
/* Any lane of `v`, lanes `w` bytes wide. */
GEX_INLINE int gex_mx_tiny_v(v128_t v, uint32_t w) {
    if (w == 4) {
        v128_t t = wasm_u32x4_shr(wasm_i32x4_sub(wasm_v128_and(v, wasm_i32x4_splat(0x7fffffff)), wasm_i32x4_splat(1)), 23);
        return wasm_v128_any_true(wasm_i32x4_eq(t, wasm_i32x4_splat(0)));
    }
    v128_t t = wasm_u64x2_shr(wasm_i64x2_sub(wasm_v128_and(v, wasm_i64x2_splat(0x7fffffffffffffffll)), wasm_i64x2_splat(1)), 52);
    return wasm_v128_any_true(wasm_i64x2_eq(t, wasm_i64x2_splat(0)));
}

GEX_INLINE uint64_t gex_mx_scalar(int ctx, int32_t code, uint64_t a, uint64_t b, uint32_t mx) {
    return (uint64_t)gex_mx(ctx, code, (int64_t)a, (int64_t)b, (int32_t)mx);
}

/* Lane `k` (a constant) of `v`, `w` bytes wide, as the import's operand. */
#define GEX_MX_IN(v, k, w) \
    ((w) == 4 ? (uint64_t)(uint32_t)wasm_i32x4_extract_lane(v, k) : (uint64_t)wasm_i64x2_extract_lane(v, (k) & 1))

GEX_INLINE v128_t gex_mx_lanes(int ctx, int32_t code, v128_t a, v128_t b, uint32_t n, uint32_t in_bytes,
                               uint32_t out_bytes, uint32_t mx) {
    v128_t r = wasm_i64x2_const(0, 0);
#define GEX_MX_LANE(k)                                                                                       \
    if ((k) < n) {                                                                                           \
        uint64_t x = gex_mx_scalar(ctx, code, GEX_MX_IN(a, k, in_bytes), GEX_MX_IN(b, k, in_bytes), mx);    \
        r = out_bytes == 4 ? wasm_i32x4_replace_lane(r, k, (int32_t)x) : wasm_i64x2_replace_lane(r, (k) & 1, (int64_t)x); \
    }
    GEX_MX_LANE(0)
    GEX_MX_LANE(1)
    GEX_MX_LANE(2)
    GEX_MX_LANE(3)
#undef GEX_MX_LANE
    return r;
}
