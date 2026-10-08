
/* aarch64: glibc's strcmp (sysdeps/aarch64/strcmp.S) step for step. Its
 * result is not the difference of the first differing bytes: the end takes
 * the top byte of each word shifted to the highest set bit of the syndrome
 * (difference | first string's NUL marks), so 'a' against 'b' or a NUL in
 * the second string where the strings are mutually misaligned gives another
 * value of the same sign. Programs see this one, so the provider returns
 * it (a later GEX_PROVIDE of an export replaces the earlier one). */
#define A64_ONES 0x0101010101010101ull
#define A64_7F 0x7f7f7f7f7f7f7f7full
#define A64_NUL(v) (((v) - A64_ONES) & ~((v) | A64_7F))
GEX_INLINE uint64_t a64_strcmp_end(uint64_t syn, uint64_t d1, uint64_t d2) {
    syn = __builtin_bswap64(syn), d1 = __builtin_bswap64(d1), d2 = __builtin_bswap64(d2);
    unsigned sh = (syn ? (unsigned)__builtin_clzll(syn) : 64u) & 63u;
    return ((d1 << sh) >> 56) - ((d2 << sh) >> 56);
}
static GEX_PROVIDER(p_strcmp_a64) {
    (void)depth, (void)need;
    uint64_t s1 = gp64_arg(ctx, 0), s2 = gp64_arg(ctx, 1);
    uint64_t off = s2 - s1, d1, d2, nul;
    if (!(off & 7u)) {
        if (s1 & 7u) {
            unsigned sh = (unsigned)(-(s2 << 3)) & 63u;
            uint64_t pre = ~0ull >> sh;
            s1 &= ~7ull;
            d2 = GW64(s1 + off) | pre, d1 = GW64(s1) | pre;
            s1 += 8;
        } else {
            d2 = GW64(s1 + off), d1 = GW64(s1);
            s1 += 8;
        }
        while (!(nul = A64_NUL(d1)) && d1 == d2) {
            d2 = GW64(s1 + off), d1 = GW64(s1);
            s1 += 8;
        }
        return gp64_return_rax(ctx, ret, a64_strcmp_end((d1 ^ d2) | nul, d1, d2));
    }
    while (s1 & 7u) {
        uint64_t c1 = B(s1), c2 = B(s2);
        ++s1, ++s2;
        if (!c1 || c1 != c2) {
            return gp64_return_rax(ctx, ret, c1 - c2);
        }
    }
    /* s1 aligned, s2 not: the aligned word of s2 ahead says where its NUL
     * is before an unaligned load could cross a page. */
    unsigned sh = (unsigned)(-(s2 << 3)) & 63u;
    uint64_t a2 = s2 & ~7ull, w2 = GW64(a2) | (A64_ONES >> sh);
    a2 += 8;
    uint64_t n2 = A64_NUL(w2);
    if (!n2) {
        uint64_t ahead = a2 - s1;
        for (;;) {
            w2 = GW64(s1 + ahead);
            d2 = GW64(s1 + off);
            n2 = A64_NUL(w2);
            d1 = GW64(s1);
            s1 += 8;
            if (n2 || d1 != d2) {
                break;
            }
        }
        uint64_t syn = (d1 ^ d2) | (n2 << sh);
        if (syn) {
            return gp64_return_rax(ctx, ret, a64_strcmp_end(syn, d1, d2));
        }
    }
    unsigned rs = (unsigned)(-sh) & 63u;
    d1 = GW64(s1);
    d2 = w2 >> rs;
    return gp64_return_rax(ctx, ret, a64_strcmp_end((d1 ^ d2) | (n2 >> rs), d1, d2));
}
GEX_PROVIDE(p_strcmp_a64, "strcmp")
