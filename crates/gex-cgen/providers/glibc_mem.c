/* PROVIDES libc.so.6 x86_64
 *
 * glibc's mem* and str* functions (x86-64) as native wasm
 * (docs/elf-providers.md §5): the first ELF provider. memcpy, memmove and
 * memset are IFUNCs on x86-64; the host binds their resolvers to
 * `gexp__ifunc`. Bulk copies and fills are wasm memory.copy and
 * memory.fill. Results are what glibc's x86-64 implementations return:
 * memcmp and strcmp give the difference of the first differing bytes (as
 * unsigned char), in eax (rax zero-extended, as a 32-bit subtraction
 * leaves it). None of them sets errno. The _chk forms (a bounds argument
 * and __chk_fail) stay translated. */

GEX_ELF_IFUNC

#define B(a) (*(gex_u8 *)GP64_P(a))
#define ONES 0x0101010101010101ull
#define HIGHS 0x8080808080808080ull
/* The lowest set bit marks the first zero byte of `v` (false positives only
 * above a true zero). */
GEX_INLINE uint64_t zero_bytes(uint64_t v) { return (v - ONES) & ~v & HIGHS; }

static GEX_PROVIDER(p_memcpy) {
    (void)depth, (void)need;
    uint64_t d = gp64_arg(ctx, 0), s = gp64_arg(ctx, 1), n = gp64_arg(ctx, 2);
    if (n) {
        __builtin_memmove(GP64_P(d), GP64_P(s), (uintptr_t)n);
    }
    return gp64_return_rax(ctx, ret, d);
}
static GEX_PROVIDER(p_mempcpy) {
    (void)depth, (void)need;
    uint64_t d = gp64_arg(ctx, 0), s = gp64_arg(ctx, 1), n = gp64_arg(ctx, 2);
    if (n) {
        __builtin_memmove(GP64_P(d), GP64_P(s), (uintptr_t)n);
    }
    return gp64_return_rax(ctx, ret, d + n);
}
static GEX_PROVIDER(p_memset) {
    (void)depth, (void)need;
    uint64_t d = gp64_arg(ctx, 0), c = gp64_arg(ctx, 1), n = gp64_arg(ctx, 2);
    if (n) {
        __builtin_memset(GP64_P(d), (int)(uint8_t)c, (uintptr_t)n);
    }
    return gp64_return_rax(ctx, ret, d);
}
static GEX_PROVIDER(p_memcmp) {
    (void)depth, (void)need;
    uint64_t a = gp64_arg(ctx, 0), b = gp64_arg(ctx, 1), n = gp64_arg(ctx, 2);
    uint32_t r = 0;
    while (n >= 8) {
        uint64_t x = GW64(a), y = GW64(b);
        if (x != y) {
            unsigned sh = (unsigned)__builtin_ctzll(x ^ y) & ~7u;
            r = (uint32_t)((int)(uint8_t)(x >> sh) - (int)(uint8_t)(y >> sh));
            return gp64_return_rax(ctx, ret, r);
        }
        a += 8, b += 8, n -= 8;
    }
    for (; n; --n, ++a, ++b) {
        uint8_t x = B(a), y = B(b);
        if (x != y) {
            r = (uint32_t)((int)x - (int)y);
            break;
        }
    }
    return gp64_return_rax(ctx, ret, r);
}
static GEX_PROVIDER(p_memchr) {
    (void)depth, (void)need;
    uint64_t s = gp64_arg(ctx, 0), n = gp64_arg(ctx, 2);
    uint8_t c = (uint8_t)gp64_arg(ctx, 1);
    uint64_t r = 0;
    for (; n; --n, ++s) {
        if (B(s) == c) {
            r = s;
            break;
        }
    }
    return gp64_return_rax(ctx, ret, r);
}
static GEX_PROVIDER(p_strlen) {
    (void)depth, (void)need;
    uint64_t s = gp64_arg(ctx, 0), p = s & ~7ull;
    /* Aligned words: a read never crosses a page, so never past the end of
     * memory; the bytes before `s` are forced non-zero. */
    uint64_t w = GW64(p) | ((1ull << (8u * (unsigned)(s & 7u))) - 1u), z;
    while (!(z = zero_bytes(w))) {
        p += 8;
        w = GW64(p);
    }
    return gp64_return_rax(ctx, ret, p + ((unsigned)__builtin_ctzll(z) >> 3) - s);
}
static GEX_PROVIDER(p_strchr) {
    (void)depth, (void)need;
    uint64_t s = gp64_arg(ctx, 0);
    uint8_t c = (uint8_t)gp64_arg(ctx, 1);
    for (;; ++s) {
        uint8_t x = B(s);
        if (x == c) {
            break;
        }
        if (!x) {
            s = 0;
            break;
        }
    }
    return gp64_return_rax(ctx, ret, s);
}
static GEX_PROVIDER(p_strcmp) {
    (void)depth, (void)need;
    uint64_t a = gp64_arg(ctx, 0), b = gp64_arg(ctx, 1);
    uint32_t r;
    for (;; ++a, ++b) {
        uint8_t x = B(a), y = B(b);
        if (x != y || !x) {
            r = (uint32_t)((int)x - (int)y);
            break;
        }
    }
    return gp64_return_rax(ctx, ret, r);
}

GEX_PROVIDE(p_memcpy, "memcpy")
GEX_PROVIDE(p_memcpy, "memmove")
GEX_PROVIDE(p_mempcpy, "mempcpy")
GEX_PROVIDE(p_mempcpy, "__mempcpy")
GEX_PROVIDE(p_memset, "memset")
GEX_PROVIDE(p_memcmp, "memcmp")
GEX_PROVIDE(p_memcmp, "bcmp")
GEX_PROVIDE(p_memcmp, "__memcmpeq")
GEX_PROVIDE(p_memchr, "memchr")
GEX_PROVIDE(p_strlen, "strlen")
GEX_PROVIDE(p_strchr, "strchr")
GEX_PROVIDE(p_strcmp, "strcmp")
