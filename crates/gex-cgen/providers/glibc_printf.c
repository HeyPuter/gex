/* PROVIDES libc.so.6 x86_64 stateful
 *
 * glibc's string printf family (x86-64) as native wasm
 * (docs/elf-providers.md §5): snprintf, vsnprintf, sprintf, vsprintf. The
 * formatting is musl's printf core (src/stdio/vfprintf.c, MIT licence),
 * its floating-point conversion on double (exact, as glibc's is), with
 * glibc's conventions where the two differ: "(nil)" for a null %p, "(null)"
 * for a null %s unless the precision is under 6.
 *
 * What it does not do itself it hands to glibc, by calling the library's
 * own __vsnprintf_chk / __vsprintf_chk (never bound here) with the
 * arguments as they came: positional arguments (%1$d), %m, wide
 * characters and strings (%lc, %ls, %C, %S), %a, long double (%Lf), %#g,
 * the glibc-only I flag, and every floating-point conversion while LC_NUMERIC's
 * decimal point is not "." (asked of the library's localeconv). The
 * fortified forms (__snprintf_chk...) stay the library's. */

typedef __SIZE_TYPE__ size_t;

#define MAX(a, b) ((a) > (b) ? (a) : (b))
#define MIN(a, b) ((a) < (b) ? (a) : (b))
#define INT_MAX 0x7fffffff

#define ALT_FORM (1U << ('#' - ' '))
#define ZERO_PAD (1U << ('0' - ' '))
#define LEFT_ADJ (1U << ('-' - ' '))
#define PAD_POS (1U << (' ' - ' '))
#define MARK_POS (1U << ('+' - ' '))
#define GROUPED (1U << ('\'' - ' '))
#define FLAGMASK (ALT_FORM | ZERO_PAD | LEFT_ADJ | PAD_POS | MARK_POS | GROUPED)

/* ---- the guest's arguments: a SysV va_list ---- */
struct va {
    uint32_t gp, fp;
    uint64_t over;
    uint64_t gpr[6];
    uint64_t xmm[8];
};
static uint64_t va_int(struct va *v) {
    if (v->gp < 48) {
        uint64_t x = v->gpr[v->gp / 8];
        v->gp += 8;
        return x;
    }
    uint64_t x = GW64(v->over);
    v->over += 8;
    return x;
}
static double va_dbl(struct va *v) {
    uint64_t b;
    if (v->fp < 176) {
        b = v->xmm[(v->fp - 48) / 16];
        v->fp += 16;
    } else {
        b = GW64(v->over);
        v->over += 8;
    }
    return gex_f64(b);
}

/* ---- the output: a guest buffer of `cap` bytes ---- */
struct out {
    uint64_t buf, cap, len;
};
static void out(struct out *o, const char *s, size_t l) {
    if (o->len < o->cap) {
        size_t k = MIN(l, (size_t)(o->cap - o->len));
        __builtin_memcpy(GP64_P(o->buf + o->len), s, k);
    }
    o->len += l;
}
static void out_guest(struct out *o, uint64_t s, size_t l) {
    if (o->len < o->cap) {
        size_t k = MIN(l, (size_t)(o->cap - o->len));
        __builtin_memmove(GP64_P(o->buf + o->len), GP64_P(s), k);
    }
    o->len += l;
}
static void pad(struct out *o, char c, int w, int l, int fl) {
    char p[64];
    if (fl & (LEFT_ADJ | ZERO_PAD) || l >= w) {
        return;
    }
    l = w - l;
    __builtin_memset(p, c, sizeof p);
    for (; l >= (int)sizeof p; l -= sizeof p) {
        out(o, p, sizeof p);
    }
    out(o, p, l);
}

static const char xdigits[16] = "0123456789ABCDEF";

static char *fmt_x(uint64_t x, char *s, int lower) {
    for (; x; x >>= 4) {
        *--s = xdigits[(x & 15)] | lower;
    }
    return s;
}
static char *fmt_o(uint64_t x, char *s) {
    for (; x; x >>= 3) {
        *--s = '0' + (x & 7);
    }
    return s;
}
static char *fmt_u(uint64_t x, char *s) {
    for (; x; x /= 10) {
        *--s = '0' + x % 10;
    }
    return s;
}

/* frexp without libm: y = m * 2^e, 0.5 <= |m| < 1. */
static double frexp_(double y, int *e) {
    uint64_t b = gex_b64(y);
    int ee = (int)(b >> 52 & 0x7ff);
    if (!ee) {
        if (y) {
            y = frexp_(y * 0x1p64, e);
            *e -= 64;
        } else {
            *e = 0;
        }
        return y;
    }
    *e = ee - 0x3fe;
    b &= 0x800fffffffffffffull;
    b |= 0x3fe0000000000000ull;
    return gex_f64(b);
}

#define DBL_MANT_DIG 53
#define DBL_MAX_EXP 1024
#define DBL_EPSILON 0x1p-52

/* musl's fmt_fp, on double. */
static int fmt_fp(struct out *f, double y, int w, int p, int fl, int t) {
    uint32_t big[(DBL_MANT_DIG + 28) / 29 + 1 + (DBL_MAX_EXP + DBL_MANT_DIG + 28 + 8) / 9];
    uint32_t *a, *d, *r, *z;
    int e2 = 0, e, i, j, l;
    char buf[9 + DBL_MANT_DIG / 4];
    const char *prefix = "-0X+0X 0X-0x+0x 0x";
    int pl;
    char ebuf0[3 * sizeof(int)], *ebuf = &ebuf0[3 * sizeof(int)], *estr = ebuf;

    pl = 1;
    if (__builtin_signbit(y)) {
        y = -y;
    } else if (fl & MARK_POS) {
        prefix += 3;
    } else if (fl & PAD_POS) {
        prefix += 6;
    } else {
        prefix++, pl = 0;
    }

    if (!__builtin_isfinite(y)) {
        const char *s = (t & 32) ? "inf" : "INF";
        if (y != y) {
            s = (t & 32) ? "nan" : "NAN";
        }
        pad(f, ' ', w, 3 + pl, fl & ~ZERO_PAD);
        out(f, prefix, pl);
        out(f, s, 3);
        pad(f, ' ', w, 3 + pl, fl ^ LEFT_ADJ);
        return MAX(w, 3 + pl);
    }

    y = frexp_(y, &e2) * 2;
    if (y) {
        e2--;
    }
    if (p < 0) {
        p = 6;
    }
    if (y) {
        y *= 0x1p28, e2 -= 28;
    }
    if (e2 < 0) {
        a = r = z = big;
    } else {
        a = r = z = big + sizeof(big) / sizeof(*big) - DBL_MANT_DIG - 1;
    }
    do {
        *z = (uint32_t)y;
        y = 1000000000 * (y - *z++);
    } while (y);

    while (e2 > 0) {
        uint32_t carry = 0;
        int sh = MIN(29, e2);
        for (d = z - 1; d >= a; d--) {
            uint64_t x = ((uint64_t)*d << sh) + carry;
            *d = (uint32_t)(x % 1000000000);
            carry = (uint32_t)(x / 1000000000);
        }
        if (carry) {
            *--a = carry;
        }
        while (z > a && !z[-1]) {
            z--;
        }
        e2 -= sh;
    }
    while (e2 < 0) {
        uint32_t carry = 0, *b;
        int sh = MIN(9, -e2), need = 1 + (p + DBL_MANT_DIG / 3U + 8) / 9;
        for (d = a; d < z; d++) {
            uint32_t rm = *d & ((1 << sh) - 1);
            *d = (*d >> sh) + carry;
            carry = (1000000000 >> sh) * rm;
        }
        if (!*a) {
            a++;
        }
        if (carry) {
            *z++ = carry;
        }
        b = (t | 32) == 'f' ? r : a;
        if (z - b > need) {
            z = b + need;
        }
        e2 += sh;
    }

    if (a < z) {
        for (i = 10, e = 9 * (r - a); *a >= (uint32_t)i; i *= 10, e++) {
        }
    } else {
        e = 0;
    }

    j = p - ((t | 32) != 'f') * e - ((t | 32) == 'g' && p);
    if (j < 9 * (z - r - 1)) {
        uint32_t x;
        d = r + 1 + ((j + 9 * DBL_MAX_EXP) / 9 - DBL_MAX_EXP);
        j += 9 * DBL_MAX_EXP;
        j %= 9;
        for (i = 10, j++; j < 9; i *= 10, j++) {
        }
        x = *d % i;
        if (x || d + 1 != z) {
            double round = 2 / DBL_EPSILON;
            double small;
            if ((*d / i & 1) || (i == 1000000000 && d > a && (d[-1] & 1))) {
                round += 2;
            }
            if (x < (uint32_t)i / 2) {
                small = 0x0.8p0;
            } else if (x == (uint32_t)i / 2 && d + 1 == z) {
                small = 0x1.0p0;
            } else {
                small = 0x1.8p0;
            }
            if (pl && *prefix == '-') {
                round *= -1, small *= -1;
            }
            *d -= x;
            if (round + small != round) {
                *d = *d + i;
                while (*d > 999999999) {
                    *d-- = 0;
                    if (d < a) {
                        *--a = 0;
                    }
                    (*d)++;
                }
                for (i = 10, e = 9 * (r - a); *a >= (uint32_t)i; i *= 10, e++) {
                }
            }
        }
        if (z > d + 1) {
            z = d + 1;
        }
    }
    for (; z > a && !z[-1]; z--) {
    }

    if ((t | 32) == 'g') {
        if (!p) {
            p++;
        }
        if (p > e && e >= -4) {
            t--;
            p -= e + 1;
        } else {
            t -= 2;
            p--;
        }
        if (!(fl & ALT_FORM)) {
            if (z > a && z[-1]) {
                for (i = 10, j = 0; z[-1] % i == 0; i *= 10, j++) {
                }
            } else {
                j = 9;
            }
            if ((t | 32) == 'f') {
                p = MIN(p, MAX(0, 9 * (z - r - 1) - j));
            } else {
                p = MIN(p, MAX(0, 9 * (z - r - 1) + e - j));
            }
        }
    }
    if (p > INT_MAX - 1 - (p || (fl & ALT_FORM))) {
        return -1;
    }
    l = 1 + p + (p || (fl & ALT_FORM));
    if ((t | 32) == 'f') {
        if (e > INT_MAX - l) {
            return -1;
        }
        if (e > 0) {
            l += e;
        }
    } else {
        estr = fmt_u(e < 0 ? -e : e, ebuf);
        while (ebuf - estr < 2) {
            *--estr = '0';
        }
        *--estr = (e < 0 ? '-' : '+');
        *--estr = t;
        if (ebuf - estr > INT_MAX - l) {
            return -1;
        }
        l += ebuf - estr;
    }

    if (l > INT_MAX - pl) {
        return -1;
    }
    pad(f, ' ', w, pl + l, fl);
    out(f, prefix, pl);
    pad(f, '0', w, pl + l, fl ^ ZERO_PAD);

    if ((t | 32) == 'f') {
        if (a > r) {
            a = r;
        }
        for (d = a; d <= r; d++) {
            char *s = fmt_u(*d, buf + 9);
            if (d != a) {
                while (s > buf) {
                    *--s = '0';
                }
            } else if (s == buf + 9) {
                *--s = '0';
            }
            out(f, s, buf + 9 - s);
        }
        if (p || (fl & ALT_FORM)) {
            out(f, ".", 1);
        }
        for (; d < z && p > 0; d++, p -= 9) {
            char *s = fmt_u(*d, buf + 9);
            while (s > buf) {
                *--s = '0';
            }
            out(f, s, MIN(9, p));
        }
        pad(f, '0', p + 9, 9, 0);
    } else {
        if (z <= a) {
            z = a + 1;
        }
        for (d = a; d < z && p >= 0; d++) {
            char *s = fmt_u(*d, buf + 9);
            if (s == buf + 9) {
                *--s = '0';
            }
            if (d != a) {
                while (s > buf) {
                    *--s = '0';
                }
            } else {
                out(f, s++, 1);
                if (p > 0 || (fl & ALT_FORM)) {
                    out(f, ".", 1);
                }
            }
            out(f, s, MIN(buf + 9 - s, p));
            p -= buf + 9 - s;
        }
        pad(f, '0', p + 18, 18, 0);
        out(f, estr, ebuf - estr);
    }

    pad(f, ' ', w, pl + l, fl ^ LEFT_ADJ);
    return MAX(w, pl + l);
}

static int isdigit_(int c) { return (unsigned)c - '0' < 10; }
#define CH(p) (*(const unsigned char *)GP64_P(p))

static int getint(uint64_t *s) {
    int i;
    for (i = 0; isdigit_(CH(*s)); (*s)++) {
        if (i > INT_MAX / 10 || CH(*s) - '0' > INT_MAX - 10 * i) {
            i = -1;
        } else {
            i = 10 * i + (CH(*s) - '0');
        }
    }
    return i;
}

/* Why the core gave up: glibc does it instead (also for every error:
 * EOVERFLOW, a malformed conversion, so errno and the result are glibc's). */
#define GIVE_UP (-2)

/* The printf core. `dp_ok`: LC_NUMERIC's decimal point is ".", asked once
 * per call at the first floating-point conversion (-1: not yet). */
struct fmtctx {
    int ctx;
    int32_t depth;
    uint64_t sp_top;
    int dp_ok;
};

static int decimal_point_ok(struct fmtctx *c);

static int printf_core(struct out *f, uint64_t fmt, struct va *ap, struct fmtctx *c) {
    uint64_t s = fmt;
    unsigned l10n = 0, fl;
    int w, p, xp;
    int cnt = 0, l = 0;
    char buf[sizeof(uint64_t) * 3 + 3];
    const char *prefix;
    int t, pl;

    for (;;) {
        if (l > INT_MAX - cnt) {
            return GIVE_UP; /* EOVERFLOW: glibc says so */
        }
        cnt += l;
        if (!CH(s)) {
            break;
        }
        /* The literal text up to the next %, %% as one %. */
        uint64_t a, z;
        for (a = s; CH(s) && CH(s) != '%'; s++) {
        }
        for (z = s; CH(s) == '%' && CH(s + 1) == '%'; z++, s += 2) {
        }
        if (z - a > (uint64_t)(INT_MAX - cnt)) {
            return GIVE_UP;
        }
        l = (int)(z - a);
        out_guest(f, a, l);
        if (l) {
            continue;
        }
        s++; /* past the % */
        if (isdigit_(CH(s)) && CH(s + 1) == '$') {
            return GIVE_UP; /* positional */
        }
        /* Flags. */
        for (fl = 0; (unsigned)CH(s) - ' ' < 32 && (FLAGMASK & (1U << (CH(s) - ' '))); s++) {
            fl |= 1U << (CH(s) - ' ');
        }
        if (CH(s) == 'I') {
            return GIVE_UP;
        }
        /* Width. */
        if (CH(s) == '*') {
            if (isdigit_(CH(s + 1)) && CH(s + 2) == '$') {
                return GIVE_UP;
            }
            s++;
            w = (int)(int32_t)va_int(ap);
            if (w < 0) {
                fl |= LEFT_ADJ, w = -w;
            }
        } else if ((w = getint(&s)) < 0) {
            return GIVE_UP;
        } else if (CH(s) == '$') {
            return GIVE_UP;
        }
        /* Left adjustment wins over zero padding (a negative `*` width). */
        if (fl & LEFT_ADJ) {
            fl &= ~ZERO_PAD;
        }
        /* Precision. */
        if (CH(s) == '.' && CH(s + 1) == '*') {
            if (isdigit_(CH(s + 2)) && CH(s + 3) == '$') {
                return GIVE_UP;
            }
            s += 2;
            p = (int)(int32_t)va_int(ap);
            xp = (p >= 0);
        } else if (CH(s) == '.') {
            s++;
            p = getint(&s);
            xp = 1;
        } else {
            p = -1;
            xp = 0;
        }
        /* Length modifier. */
        int len = 0; /* 0 int, 1 h, 2 hh, 3 l/ll/j/z/t/q */
        int big_l = 0;
        for (;;) {
            unsigned ch = CH(s);
            if (ch == 'h') {
                len = len == 1 ? 2 : 1;
            } else if (ch == 'l' || ch == 'j' || ch == 'z' || ch == 'Z' || ch == 't' || ch == 'q') {
                len = 3;
            } else if (ch == 'L') {
                len = 3, big_l = 1;
            } else {
                break;
            }
            s++;
        }
        t = CH(s);
        s++;
        if (!t) {
            return GIVE_UP;
        }
        /* Not here: glibc. */
        if (t == 'm' || t == 'C' || t == 'S' || t == 'a' || t == 'A' || ((t == 'c' || t == 's') && len == 3)) {
            return GIVE_UP;
        }
        prefix = "-+   0X0x";
        pl = 0;
        char *zz = buf + sizeof(buf), *aa;
        uint64_t arg;
        switch (t) {
        case 'n': {
            uint64_t ptr = va_int(ap);
            if (len == 3) {
                GW64(ptr) = (uint64_t)(int64_t)cnt;
            } else if (len == 1) {
                *(gex_u16 *)GP64_P(ptr) = (uint16_t)cnt;
            } else if (len == 2) {
                *(gex_u8 *)GP64_P(ptr) = (uint8_t)cnt;
            } else {
                *(gex_u32 *)GP64_P(ptr) = (uint32_t)cnt;
            }
            l = 0;
            continue;
        }
        case 'p': {
            arg = va_int(ap);
            if (!arg) {
                /* glibc: "(nil)", as a string. */
                l = 5;
                pad(f, ' ', w, l, fl & ~ZERO_PAD);
                out(f, "(nil)", 5);
                pad(f, ' ', w, l, fl ^ LEFT_ADJ);
                l = MAX(w, l);
                continue;
            }
            /* As %#lx. */
            aa = fmt_x(arg, zz, 32);
            prefix += 7, pl = 2;
            if (fl & MARK_POS) {
                prefix = "+0x", pl = 3;
            } else if (fl & PAD_POS) {
                prefix = " 0x", pl = 3;
            }
            goto ints;
        }
        case 'x':
        case 'X':
        case 'o':
        case 'u':
        case 'd':
        case 'i': {
            uint64_t v = va_int(ap);
            int neg = 0;
            if (t == 'd' || t == 'i') {
                int64_t sv = len == 3 ? (int64_t)v : len == 1 ? (int16_t)v : len == 2 ? (int8_t)v : (int32_t)v;
                if (sv < 0) {
                    neg = 1, v = -(uint64_t)sv;
                } else {
                    v = (uint64_t)sv;
                }
                if (neg) {
                    pl = 1;
                } else if (fl & MARK_POS) {
                    pl = 1, prefix++;
                } else if (fl & PAD_POS) {
                    pl = 1, prefix += 2;
                }
                aa = fmt_u(v, zz);
            } else {
                v = len == 3 ? v : len == 1 ? (uint16_t)v : len == 2 ? (uint8_t)v : (uint32_t)v;
                if (t == 'o') {
                    aa = fmt_o(v, zz);
                    if ((fl & ALT_FORM) && p < zz - aa + 1) {
                        p = zz - aa + 1;
                    }
                } else if (t == 'u') {
                    aa = fmt_u(v, zz);
                } else {
                    aa = fmt_x(v, zz, t & 32);
                    if (v && (fl & ALT_FORM)) {
                        prefix += (t >> 4), pl = 2;
                    }
                }
            }
            arg = v;
        ints:
            if (xp && p < 0) {
                return GIVE_UP;
            }
            if (xp) {
                fl &= ~ZERO_PAD;
            }
            if (!arg && !p) {
                aa = zz;
                l = 0;
            } else {
                p = MAX(p, zz - aa + !arg);
            }
            if (p > INT_MAX - pl) {
                return GIVE_UP;
            }
            if (p < zz - aa) {
                p = zz - aa;
            }
            if (w < pl + p) {
                w = pl + p;
            }
            if (w > INT_MAX - cnt) {
                return GIVE_UP;
            }
            pad(f, ' ', w, pl + p, fl);
            out(f, prefix, pl);
            pad(f, '0', w, pl + p, fl ^ ZERO_PAD);
            pad(f, '0', p, zz - aa, 0);
            out(f, aa, zz - aa);
            pad(f, ' ', w, pl + p, fl ^ LEFT_ADJ);
            l = w;
            continue;
        }
        case 'c': {
            char ch = (char)va_int(ap);
            pad(f, ' ', w, 1, fl & ~ZERO_PAD);
            out(f, &ch, 1);
            pad(f, ' ', w, 1, fl ^ LEFT_ADJ);
            l = MAX(w, 1);
            continue;
        }
        case 's': {
            uint64_t str = va_int(ap);
            if (!str) {
                /* glibc: "(null)", or nothing if it would not fit whole. */
                l = (p < 0 || p >= 6) ? 6 : 0;
                pad(f, ' ', w, l, fl & ~ZERO_PAD);
                out(f, "(null)", l);
                pad(f, ' ', w, l, fl ^ LEFT_ADJ);
                l = MAX(w, l);
                continue;
            }
            uint64_t e = str;
            if (p < 0) {
                while (CH(e)) {
                    e++;
                }
            } else {
                while (e - str < (uint64_t)p && CH(e)) {
                    e++;
                }
            }
            if (e - str > (uint64_t)INT_MAX) {
                return GIVE_UP;
            }
            l = (int)(e - str);
            pad(f, ' ', w, l, fl & ~ZERO_PAD);
            out_guest(f, str, l);
            pad(f, ' ', w, l, fl ^ LEFT_ADJ);
            l = MAX(w, l);
            continue;
        }
        case 'e':
        case 'f':
        case 'g':
        case 'E':
        case 'F':
        case 'G': {
            /* long double; and %#g, where glibc's own rounding across a
             * power of ten drops digits ("1.e+06" for 999999.5), which
             * glibc is left to do. */
            if (big_l || ((t | 32) == 'g' && (fl & ALT_FORM))) {
                return GIVE_UP;
            }
            if (c->dp_ok < 0) {
                c->dp_ok = decimal_point_ok(c);
            }
            if (!c->dp_ok) {
                return GIVE_UP;
            }
            double y = va_dbl(ap);
            if (xp && p < 0) {
                return GIVE_UP;
            }
            l = fmt_fp(f, y, w, p, fl, t);
            if (l < 0) {
                return GIVE_UP;
            }
            continue;
        }
        default:
            /* An unknown conversion: glibc prints it as it stands. */
            return GIVE_UP;
        }
    }
    return cnt;
}

/* ---- the library's own functions, found by name ---- */

/* The ELF image holding `addr`: the page-aligned "\x7fELF" at or below it
 * (64 MiB at most), 0 if none. */
static uint64_t elf_base(uint64_t addr) {
    for (uint64_t b = addr & ~0xfffull, n = 0; n < 16384 && b; b -= 0x1000, ++n) {
        if (*(gex_u32 *)GP64_P(b) == 0x464c457fu) {
            return b;
        }
    }
    return 0;
}

/* The address of dynamic symbol `name` of the image at `base` (its
 * DT_SYMTAB, DT_STRTAB and DT_GNU_HASH's symbol count), or 0. */
static uint64_t elf_sym(uint64_t base, const char *name) {
    uint64_t ph = base + GW64(base + 0x20);
    unsigned phn = *(gex_u16 *)GP64_P(base + 0x38);
    uint64_t dyn = 0, vbias = 0;
    int first = 1;
    for (unsigned i = 0; i < phn; ++i, ph += 56) {
        uint32_t type = *(gex_u32 *)GP64_P(ph);
        if (type == 1 && first) {
            vbias = base - (GW64(ph + 16) & ~0xfffull); /* the first PT_LOAD's place */
            first = 0;
        }
        if (type == 2) {
            dyn = GW64(ph + 16);
        }
    }
    if (!dyn) {
        return 0;
    }
    dyn += vbias;
    uint64_t symtab = 0, strtab = 0, gnuhash = 0;
    for (uint64_t d = dyn; GW64(d); d += 16) {
        uint64_t tag = GW64(d), val = GW64(d + 8);
        /* The dynamic linker relocates these in place; before it has,
         * they are link-time addresses. */
        if (val < base) {
            val += vbias;
        }
        if (tag == 6) {
            symtab = val;
        } else if (tag == 5) {
            strtab = val;
        } else if (tag == 0x6ffffef5) {
            gnuhash = val;
        }
    }
    if (!symtab || !strtab || !gnuhash) {
        return 0;
    }
    /* The symbol count: past the last bucket's chain. */
    uint32_t nbuckets = *(gex_u32 *)GP64_P(gnuhash), symoffset = *(gex_u32 *)GP64_P(gnuhash + 4);
    uint32_t bloom = *(gex_u32 *)GP64_P(gnuhash + 8);
    uint64_t buckets = gnuhash + 16 + 8ull * bloom, chain = buckets + 4ull * nbuckets;
    uint32_t last = 0;
    for (uint32_t i = 0; i < nbuckets; ++i) {
        last = MAX(last, *(gex_u32 *)GP64_P(buckets + 4ull * i));
    }
    uint32_t nsyms = last;
    if (last >= symoffset) {
        while (!(*(gex_u32 *)GP64_P(chain + 4ull * (nsyms - symoffset)) & 1)) {
            nsyms++;
        }
        nsyms++;
    }
    for (uint32_t i = 0; i < nsyms; ++i) {
        uint64_t sym = symtab + 24ull * i;
        uint32_t n = *(gex_u32 *)GP64_P(sym);
        uint64_t v = GW64(sym + 8);
        if (!v) {
            continue;
        }
        const char *q = name;
        uint64_t k = strtab + n;
        while (*q && CH(k) == (unsigned char)*q) {
            ++q, ++k;
        }
        if (!*q && !CH(k)) {
            return v + vbias;
        }
    }
    return 0;
}

/* The library's functions this needs, found once (per process: the data is
 * the process's). */
static uint64_t lib_base, f_vsnprintf_chk, f_vsprintf_chk, f_localeconv;

static void find(uint64_t self) {
    if (lib_base) {
        return;
    }
    uint64_t b = elf_base(self);
    f_vsnprintf_chk = b ? elf_sym(b, "__vsnprintf_chk") : 0;
    f_vsprintf_chk = b ? elf_sym(b, "__vsprintf_chk") : 0;
    f_localeconv = b ? elf_sym(b, "localeconv") : 0;
    lib_base = b ? b : 1;
}

static int decimal_point_ok(struct fmtctx *c) {
    if (!f_localeconv) {
        return 0;
    }
    uint64_t lc = gp64_call6(c->ctx, c->depth, f_localeconv, c->sp_top, 0, 0, 0, 0, 0, 0);
    uint64_t dp = lc ? GW64(lc) : 0;
    return dp && CH(dp) == '.' && CH(dp + 1) == 0;
}

/* ---- the exports ---- */

/* The arguments of a variadic entry: its `fixed` named ones first. */
static void va_regs(int ctx, struct va *v, unsigned fixed) {
    v->gpr[0] = gp64_reg(ctx, R_RDI);
    v->gpr[1] = gp64_reg(ctx, R_RSI);
    v->gpr[2] = gp64_reg(ctx, R_RDX);
    v->gpr[3] = gp64_reg(ctx, R_RCX);
    v->gpr[4] = gp64_reg(ctx, R_R8);
    v->gpr[5] = gp64_reg(ctx, R_R9);
    for (unsigned i = 0; i < 8; ++i) {
        v->xmm[i] = GEX_C64(GEX_CTX_VN(i));
    }
    v->gp = 8 * fixed;
    v->fp = 48;
    v->over = gp64_reg(ctx, R_RSP) + 8;
}

/* A guest va_list at `ap`, read (never written: glibc's caller does not
 * look at it again). */
static void va_guest(struct va *v, uint64_t ap) {
    v->gp = *(gex_u32 *)GP64_P(ap);
    v->fp = *(gex_u32 *)GP64_P(ap + 4);
    v->over = GW64(ap + 8);
    uint64_t save = GW64(ap + 16);
    for (unsigned i = 0; i < 6; ++i) {
        v->gpr[i] = v->gp < 48 ? GW64(save + 8 * i) : 0;
    }
    for (unsigned i = 0; i < 8; ++i) {
        v->xmm[i] = v->fp < 176 ? GW64(save + 48 + 16 * i) : 0;
    }
}

/* A variadic entry's arguments as a guest va_list, built on the guest's
 * stack below its RSP (and its red zone), for glibc's v* function. Returns
 * the va_list's address; *top is below everything it used. */
static uint64_t va_build(int ctx, unsigned fixed, uint64_t *top) {
    uint64_t rsp = gp64_reg(ctx, R_RSP);
    uint64_t save = ((rsp - 128 - 176) & ~15ull), ap = save - 32;
    static const unsigned char regs[6] = {R_RDI, R_RSI, R_RDX, R_RCX, R_R8, R_R9};
    for (unsigned i = 0; i < 6; ++i) {
        GW64(save + 8 * i) = gp64_reg(ctx, regs[i]);
    }
    for (unsigned i = 0; i < 8; ++i) {
        GW64(save + 48 + 16 * i) = GEX_C64(GEX_CTX_VN(i));
        GW64(save + 56 + 16 * i) = GEX_C64(GEX_CTX_VN(i) + 8);
    }
    *(gex_u32 *)GP64_P(ap) = 8 * fixed;
    *(gex_u32 *)GP64_P(ap + 4) = 48;
    GW64(ap + 8) = rsp + 8;
    GW64(ap + 16) = save;
    *top = ap;
    return ap;
}

/* Format into the guest's `buf` of `n` bytes (NUL-terminated within them
 * when n > 0); the length it would have, -1, or GIVE_UP. */
static int format(int ctx, int32_t depth, uint64_t sp_top, uint64_t buf, uint64_t n, uint64_t fmt, struct va *v) {
    struct out o = {buf, n ? n - 1 : 0, 0};
    struct fmtctx c = {ctx, depth, sp_top, -1};
    int r = printf_core(&o, fmt, v, &c);
    if (r == GIVE_UP) {
        return r;
    }
    if (n) {
        *(gex_u8 *)GP64_P(buf + MIN(o.len, n - 1)) = 0;
    }
    return r;
}

static GEX_PROVIDER(p_snprintf) {
    (void)need;
    uint64_t self = GP_SELF_PC();
    find(self);
    uint64_t s = gp64_arg(ctx, 0), n = gp64_arg(ctx, 1), fmt = gp64_arg(ctx, 2);
    struct va v;
    va_regs(ctx, &v, 3);
    int r = format(ctx, depth, gp64_reg(ctx, R_RSP) - 128, s, n, fmt, &v);
    if (r == GIVE_UP && f_vsnprintf_chk) {
        uint64_t top, ap = va_build(ctx, 3, &top);
        r = (int)gp64_call6(ctx, depth, f_vsnprintf_chk, top, s, n, 0, n, fmt, ap);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)r);
}
static GEX_PROVIDER(p_vsnprintf) {
    (void)need;
    find(GP_SELF_PC());
    uint64_t s = gp64_arg(ctx, 0), n = gp64_arg(ctx, 1), fmt = gp64_arg(ctx, 2), ap = gp64_arg(ctx, 3);
    struct va v;
    va_guest(&v, ap);
    int r = format(ctx, depth, gp64_reg(ctx, R_RSP) - 128, s, n, fmt, &v);
    if (r == GIVE_UP && f_vsnprintf_chk) {
        r = (int)gp64_call6(ctx, depth, f_vsnprintf_chk, gp64_reg(ctx, R_RSP), s, n, 0, n, fmt, ap);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)r);
}
static GEX_PROVIDER(p_sprintf) {
    (void)need;
    find(GP_SELF_PC());
    uint64_t s = gp64_arg(ctx, 0), fmt = gp64_arg(ctx, 1);
    struct va v;
    va_regs(ctx, &v, 2);
    int r = format(ctx, depth, gp64_reg(ctx, R_RSP) - 128, s, ~0ull >> 1, fmt, &v);
    if (r == GIVE_UP && f_vsprintf_chk) {
        uint64_t top, ap = va_build(ctx, 2, &top);
        r = (int)gp64_call6(ctx, depth, f_vsprintf_chk, top, s, 0, ~0ull, fmt, ap, 0);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)r);
}
static GEX_PROVIDER(p_vsprintf) {
    (void)need;
    find(GP_SELF_PC());
    uint64_t s = gp64_arg(ctx, 0), fmt = gp64_arg(ctx, 1), ap = gp64_arg(ctx, 2);
    struct va v;
    va_guest(&v, ap);
    int r = format(ctx, depth, gp64_reg(ctx, R_RSP) - 128, s, ~0ull >> 1, fmt, &v);
    if (r == GIVE_UP && f_vsprintf_chk) {
        r = (int)gp64_call6(ctx, depth, f_vsprintf_chk, gp64_reg(ctx, R_RSP), s, 0, ~0ull, fmt, ap, 0);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)r);
}

GEX_PROVIDE(p_snprintf, "snprintf")
GEX_PROVIDE(p_vsnprintf, "vsnprintf")
GEX_PROVIDE(p_sprintf, "sprintf")
GEX_PROVIDE(p_vsprintf, "vsprintf")
