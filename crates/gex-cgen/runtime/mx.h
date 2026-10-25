/* The function tier's slow path of Expr::Mx (an SSE operation under a
 * MXCSR rounding control other than to nearest): the gex.mx import, which
 * computes it exactly (gex_f80::sse::op), as tier-1's `emit_mx` calls it --
 * once for a scalar, once per lane, lanes past `n` zero.
 *
 * Out of line (GEX_MX_SLOW), so an operation's slow path is one call in
 * the C, however many lanes. The emitter tests the rounding control once
 * for a run of operations and computes the run either way: as the
 * operations' own C or as these calls.
 *
 * `f`, a vector operation's shape: bits 0-3 the lanes `n`, bit 4 an 8-byte
 * operand lane, bit 5 an 8-byte result lane.
 *
 * Included from gex_abi.h, after the imports. */

#define GEX_MX_SLOW static __attribute__((noinline, unused))

GEX_MX_SLOW uint64_t gex_mx_s(int ctx, int32_t code, uint64_t a, uint64_t b, uint32_t mx) {
    return (uint64_t)gex_mx(ctx, code, (int64_t)a, (int64_t)b, (int32_t)mx);
}

/* Lane `k` (a constant) of `v`, 8 bytes wide when `w8`, as the import's operand. */
#define GEX_MX_IN(v, k, w8) \
    (!(w8) ? (uint64_t)(uint32_t)wasm_i32x4_extract_lane(v, k) : (uint64_t)wasm_i64x2_extract_lane(v, (k) & 1))

/* An absent `b` is all zero. */
GEX_MX_SLOW v128_t gex_mx_v(int ctx, int32_t code, v128_t a, v128_t b, uint32_t f, uint32_t mx) {
    int in8 = (f & 16u) != 0, out8 = (f & 32u) != 0;
    uint32_t n = f & 15u;
    v128_t r = wasm_i64x2_const(0, 0);
#define GEX_MX_LANE(k)                                                                                    \
    if ((k) < n) {                                                                                        \
        uint64_t x = (uint64_t)gex_mx(ctx, code, (int64_t)GEX_MX_IN(a, k, in8), (int64_t)GEX_MX_IN(b, k, in8), (int32_t)mx); \
        r = !out8 ? wasm_i32x4_replace_lane(r, k, (int32_t)x) : wasm_i64x2_replace_lane(r, (k) & 1, (int64_t)x); \
    }
    GEX_MX_LANE(0)
    GEX_MX_LANE(1)
    GEX_MX_LANE(2)
    GEX_MX_LANE(3)
#undef GEX_MX_LANE
    return r;
}
