/* PROVIDES msvcr120.dll,msvcrt.dll,ucrtbase.dll i386
 *
 * The CRT's mem* and str* functions (msvcr120, msvcrt, UCRT) as native
 * wasm, in the F_body form. Bulk copies and fills
 * are wasm memory.copy and memory.fill. All are cdecl (the caller pops),
 * none sets errno; the _s forms (errno, the invalid-parameter handler) stay
 * lifted. Comparisons return -1, 0 or 1, as msvcr120's do. */

#define ARGS2 uint32_t a0 = gp_arg(ctx, 0), a1 = gp_arg(ctx, 1)
#define ARGS3 ARGS2, a2 = gp_arg(ctx, 2)
#define B(a) (*(gex_u8 *)GEX_P((uint32_t)(a)))
#define H(a) (*(gex_u16 *)GEX_P((uint32_t)(a)))

static GEX_PROVIDER(p_memcpy) {
    (void)depth, (void)need;
    ARGS3;
    if (a2) {
        __builtin_memmove(GEX_P(a0), GEX_P(a1), a2);
    }
    return gp_return_eax(ctx, ret, 0, a0);
}
static GEX_PROVIDER(p_memset) {
    (void)depth, (void)need;
    ARGS3;
    if (a2) {
        __builtin_memset(GEX_P(a0), (int)(uint8_t)a1, a2);
    }
    return gp_return_eax(ctx, ret, 0, a0);
}
static GEX_PROVIDER(p_memcmp) {
    (void)depth, (void)need;
    ARGS3;
    int r = 0;
    for (uint32_t i = 0; i < a2; ++i) {
        uint8_t x = B(a0 + i), y = B(a1 + i);
        if (x != y) {
            r = x < y ? -1 : 1;
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, (uint32_t)r);
}
static GEX_PROVIDER(p_memchr) {
    (void)depth, (void)need;
    ARGS3;
    uint32_t r = 0;
    for (uint32_t i = 0; i < a2; ++i) {
        if (B(a0 + i) == (uint8_t)a1) {
            r = a0 + i;
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(p_strlen) {
    (void)depth, (void)need;
    uint32_t s = gp_arg(ctx, 0), p = s;
    while (B(p)) {
        ++p;
    }
    return gp_return_eax(ctx, ret, 0, p - s);
}
static GEX_PROVIDER(p_strcmp) {
    (void)depth, (void)need;
    ARGS2;
    int r;
    for (;; ++a0, ++a1) {
        uint8_t x = B(a0), y = B(a1);
        if (x != y) {
            r = x < y ? -1 : 1;
            break;
        }
        if (!x) {
            r = 0;
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, (uint32_t)r);
}
static GEX_PROVIDER(p_strncmp) {
    (void)depth, (void)need;
    ARGS3;
    int r = 0;
    for (uint32_t i = 0; i < a2; ++i) {
        uint8_t x = B(a0 + i), y = B(a1 + i);
        if (x != y) {
            r = x < y ? -1 : 1;
            break;
        }
        if (!x) {
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, (uint32_t)r);
}
static GEX_PROVIDER(p_strchr) {
    (void)depth, (void)need;
    ARGS2;
    uint8_t c = (uint8_t)a1;
    uint32_t r = 0;
    for (;; ++a0) {
        uint8_t x = B(a0);
        if (x == c) {
            r = a0;
            break;
        }
        if (!x) {
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(p_strcpy) {
    (void)depth, (void)need;
    ARGS2;
    uint32_t d = a0;
    for (;; ++d, ++a1) {
        uint8_t x = B(a1);
        B(d) = x;
        if (!x) {
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, a0);
}
static GEX_PROVIDER(p_strcat) {
    (void)depth, (void)need;
    ARGS2;
    uint32_t d = a0;
    while (B(d)) {
        ++d;
    }
    for (;; ++d, ++a1) {
        uint8_t x = B(a1);
        B(d) = x;
        if (!x) {
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, a0);
}
static GEX_PROVIDER(p_wcslen) {
    (void)depth, (void)need;
    uint32_t s = gp_arg(ctx, 0), p = s;
    while (H(p)) {
        p += 2;
    }
    return gp_return_eax(ctx, ret, 0, (p - s) / 2);
}
static GEX_PROVIDER(p_wcscmp) {
    (void)depth, (void)need;
    ARGS2;
    int r;
    for (;; a0 += 2, a1 += 2) {
        uint16_t x = H(a0), y = H(a1);
        if (x != y) {
            r = x < y ? -1 : 1;
            break;
        }
        if (!x) {
            r = 0;
            break;
        }
    }
    return gp_return_eax(ctx, ret, 0, (uint32_t)r);
}

GEX_PROVIDE(p_memcpy, "memcpy")
GEX_PROVIDE(p_memcpy, "memmove")
GEX_PROVIDE(p_memset, "memset")
GEX_PROVIDE(p_memcmp, "memcmp")
GEX_PROVIDE(p_memchr, "memchr")
GEX_PROVIDE(p_strlen, "strlen")
GEX_PROVIDE(p_strcmp, "strcmp")
GEX_PROVIDE(p_strncmp, "strncmp")
GEX_PROVIDE(p_strchr, "strchr")
GEX_PROVIDE(p_strcpy, "strcpy")
GEX_PROVIDE(p_strcat, "strcat")
GEX_PROVIDE(p_wcslen, "wcslen")
GEX_PROVIDE(p_wcscmp, "wcscmp")
