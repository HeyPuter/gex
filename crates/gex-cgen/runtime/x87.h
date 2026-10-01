/* The function tier's lowering of Stmt::X87: one helper per operation,
 * fast path first, the gex.x87 import for the rest -- the same paths, and
 * the same exact results, as tier-1's `emit_x87*` (gex_f80 is the
 * reference for both, and is what the import computes).
 *
 * Each helper returns its operation's first output. The others travel the
 * way the import returns them, through the context: an extended result's
 * sign and exponent at X87_TMP + 8, the status-word bits (exception flags,
 * C1 at bit 9) at X87_TMP + 12; the caller reads them back with GEX_C32.
 * (No structs or out-pointers: nothing here may need the shadow stack.)
 *
 * Included from gex_abi.h, after the context offsets and the imports. */

#define GEX_X87_SE (GEX_CTX_X87_TMP + 8u)
#define GEX_X87_SW (GEX_CTX_X87_TMP + 12u)

/* The import, for everything the fast paths leave. */
GEX_INLINE uint64_t gex_x87_slow(int ctx, int32_t op, uint64_t asig, uint32_t ase, uint64_t bsig,
                                 uint32_t bse, uint32_t cw) {
    return (uint64_t)gex_x87(ctx, op, (int64_t)asig, (int32_t)ase, (int64_t)bsig, (int32_t)bse,
                             (int32_t)cw);
}

/* Whether to round a magnitude up: to nearest when over half an ulp or at
 * half with the kept part odd; away from zero (RC down for a negative
 * value, up for a positive one) whenever anything was dropped. */
GEX_INLINE uint32_t gex_x87_up(uint32_t cw, uint32_t sign, uint32_t rb, uint32_t st, uint32_t odd) {
    uint32_t rc = (cw >> 10) & 3;
    return ((rc == 0) & rb & (st | odd)) | ((rc == 2 - sign) & (rb | st));
}

/* fst m32 / m64 (X87Op::Narrow): normal results, every rounding mode. */
GEX_INLINE uint64_t gex_x87_narrow(int ctx, uint64_t sig, uint32_t se, uint32_t cw, uint32_t single) {
    uint32_t e = se & 0x7fff, sign = (se >> 15) & 1;
    uint32_t drop = single ? 40 : 11, frac = single ? 23 : 52, total = single ? 32 : 64;
    int32_t bias = single ? 127 : 1023;
    uint32_t lo = 16383 - (bias - 1), hi = 16383 + (bias - 1);
    if ((int64_t)sig < 0 && e - lo <= hi - lo) {
        uint64_t m = sig >> drop;
        uint64_t rem = sig & ((1ull << drop) - 1);
        uint64_t half = 1ull << (drop - 1);
        uint32_t rc = (cw >> 10) & 3;
        uint32_t up = ((rc == 0) & ((rem > half) | ((rem == half) & ((uint32_t)m & 1)))) |
                      ((rc == 2 - sign) & (rem != 0));
        uint64_t bits = ((uint64_t)(e - (16383 - bias + 1)) << frac) + m + up;
        bits |= (uint64_t)sign << (total - 1);
        GEX_C32(GEX_X87_SW) = ((uint32_t)(rem != 0) << 5) | (up << 9);
        return bits;
    }
    return gex_x87_slow(ctx, single ? 1 : 2, sig, se, 0, 0, cw);
}

/* fist / fisttp (X87Op::ToInt): zero, and 1 <= |v| < 2^(bits-1). */
GEX_INLINE uint64_t gex_x87_toint(int ctx, uint64_t sig, uint32_t se, uint32_t cw, uint32_t bits,
                                  uint32_t trunc) {
    uint32_t e = se & 0x7fff, sign = (se >> 15) & 1;
    if (sig == 0 && e == 0) {
        GEX_C32(GEX_X87_SW) = 0;
        return 0;
    }
    if ((int64_t)sig < 0 && e - 16383 <= bits - 2) {
        uint64_t sh = 16383 + 63 - e; /* 1..=63 */
        uint64_t m = sig >> sh;
        uint64_t rem = sig & ((1ull << sh) - 1);
        uint64_t half = 1ull << (sh - 1);
        uint32_t up = 0;
        if (!trunc) {
            uint32_t rc = (cw >> 10) & 3;
            up = ((rc == 0) & ((rem > half) | ((rem == half) & ((uint32_t)m & 1)))) |
                 ((rc == 2 - sign) & (rem != 0));
        }
        uint64_t k = m + up;
        if (k > (1ull << (bits - 1)) - 1 + sign) {
            GEX_C32(GEX_X87_SW) = 1; /* IE */
            return 1ull << (bits - 1);
        }
        uint64_t v = sign ? 0 - k : k;
        if (bits < 64)
            v &= (1ull << bits) - 1;
        GEX_C32(GEX_X87_SW) = ((uint32_t)(rem != 0) << 5) | (up << 9);
        return v;
    }
    return gex_x87_slow(ctx, 0x100 | (int32_t)(bits << 1) | (int32_t)trunc, sig, se, 0, 0, cw);
}

/* Round a normalised 128-bit significand hi:lo (bit 127 set), biased
 * exponent e (1 <= e <= 0x7ffd), to the precision control's 24, 53 or 64
 * bits by the rounding control; the result's sign/exponent and status bits
 * go to the context. */
GEX_INLINE uint64_t gex_x87_round(int ctx, uint64_t hi, uint64_t lo, uint32_t e, uint32_t sign, uint32_t cw) {
    uint32_t pc = cw & 0x300;
    uint64_t d = pc == 0 ? 40 : pc == 0x200 ? 11 : 0;
    uint64_t kept = hi >> d;
    uint32_t rb, st;
    if (d) {
        rb = (uint32_t)(hi >> (d - 1)) & 1;
        st = ((hi & ((1ull << (d - 1)) - 1)) | lo) != 0;
    } else {
        rb = (uint32_t)(lo >> 63);
        st = (lo << 1) != 0;
    }
    uint32_t up = gex_x87_up(cw, sign, rb, st, (uint32_t)kept & 1);
    uint32_t carry = up & (kept == (~0ull >> d));
    uint64_t sig = carry ? 1ull << 63 : (kept + up) << d;
    GEX_C32(GEX_X87_SE) = (e + carry) | sign << 15;
    GEX_C32(GEX_X87_SW) = ((rb | st) << 5) | (up << 9);
    return sig;
}

/* fadd / fsub / fmul / fdiv (X87Op::Arith, k = 0..3): add, subtract and
 * multiply inline for normal operands with a normal exact result (sums
 * with exponents at most 63 apart, an effective subtraction at least 2);
 * division and the rest through the import. */
GEX_INLINE uint64_t gex_x87_arith(int ctx, uint32_t k, uint64_t asig, uint32_t ase, uint64_t bsig,
                                  uint32_t bse, uint32_t cw) {
    uint32_t ae = ase & 0x7fff, be = bse & 0x7fff;
    if (k != 3 && (int64_t)asig < 0 && ae - 1 <= 0x7ffd && (int64_t)bsig < 0 && be - 1 <= 0x7ffd) {
        uint64_t hi, lo;
        int32_t e;
        uint32_t sign;
        int ok = 1;
        if (k == 2) {
            sign = ((ase ^ bse) >> 15) & 1;
            uint64_t al = asig & 0xffffffffull, ah = asig >> 32, bl = bsig & 0xffffffffull, bh = bsig >> 32;
            uint64_t t1 = al * bl, t2 = al * bh, t3 = ah * bl, t4 = ah * bh;
            uint64_t mid = (t1 >> 32) + (t2 & 0xffffffffull) + (t3 & 0xffffffffull);
            hi = t4 + (t2 >> 32) + (t3 >> 32) + (mid >> 32);
            lo = (t1 & 0xffffffffull) | (mid << 32);
            uint64_t top = hi >> 63, n = 1 - top;
            e = (int32_t)(ae + be) - 16383 + (int32_t)top;
            hi = (hi << n) | ((lo >> 63) & n);
            lo <<= n;
        } else {
            /* b's sign, flipped for a subtraction (a copy: the import
             * below still wants the operand as given). */
            uint32_t bs = k == 1 ? bse ^ 0x8000 : bse;
            uint32_t swap = (ae < be) | ((ae == be) & (asig < bsig));
            uint64_t sx = swap ? bsig : asig, sy = swap ? asig : bsig;
            uint32_t ex = swap ? be : ae, ey = swap ? ae : be;
            sign = ((swap ? bs : ase) >> 15) & 1;
            uint32_t sub = ((ase ^ bs) >> 15) & 1;
            uint64_t d = (uint64_t)(ex - ey);
            if (d > 63 || (sub && d < 2)) {
                ok = 0;
                hi = lo = 0;
                e = 0;
            } else {
                uint64_t yhi = sy >> d, ylo = d ? sy << (64 - d) : 0;
                e = (int32_t)ex;
                if (sub) {
                    lo = 0 - ylo;
                    hi = sx - yhi - (ylo != 0);
                    uint64_t n = (hi >> 63) ^ 1;
                    e -= (int32_t)n;
                    hi = (hi << n) | ((lo >> 63) & n);
                    lo <<= n;
                } else {
                    hi = sx + yhi;
                    uint64_t c = hi < sx;
                    lo = (ylo >> c) | ((hi << 63) * c) | (ylo & c);
                    hi = (hi >> c) | (c << 63);
                    e += (int32_t)c;
                }
            }
        }
        if (ok && (uint32_t)(e - 1) <= 0x7ffc)
            return gex_x87_round(ctx, hi, lo, (uint32_t)e, sign, cw);
    }
    return gex_x87_slow(ctx, 0x200 | (int32_t)k, asig, ase, bsig, bse, cw);
}

/* fcom / fucom (X87Op::Compare): zeros and normal numbers inline. Returns
 * the condition codes (C0 less, C3 equal, C0|C2|C3 unordered); the status
 * bits go to the context. */
GEX_INLINE uint32_t gex_x87_compare(int ctx, uint32_t quiet, uint64_t asig, uint32_t ase, uint64_t bsig,
                                    uint32_t bse) {
    uint32_t ae = ase & 0x7fff, be = bse & 0x7fff;
    int aok = (asig == 0 && ae == 0) | ((int64_t)asig < 0 && ae - 1 <= 0x7ffd);
    int bok = (bsig == 0 && be == 0) | ((int64_t)bsig < 0 && be - 1 <= 0x7ffd);
    if (aok & bok) {
        uint32_t an = ((ase >> 15) & 1) & (asig != 0), bn = ((bse >> 15) & 1) & (bsig != 0);
        uint32_t eq = (ae == be) & (asig == bsig);
        uint32_t lt = (ae < be) | ((ae == be) & (asig < bsig));
        if (an == bn)
            lt = an ? (!lt & !eq) : lt;
        else
            lt = an;
        eq &= an == bn;
        GEX_C32(GEX_X87_SW) = 0;
        return lt ? 1u << 8 : eq ? 1u << 14 : 0;
    }
    return (uint32_t)gex_x87_slow(ctx, 0x300 | (int32_t)quiet, asig, ase, bsig, bse, 0x37f);
}
