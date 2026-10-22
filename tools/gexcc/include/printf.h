/* glibc's <printf.h>, the part programs use: parse_printf_format, which
 * says what type each argument of a printf format is (systemd checks its
 * log and journal formats with it). musl has no such header. */
#ifndef _GEXCC_PRINTF_H
#define _GEXCC_PRINTF_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

enum { PA_INT, PA_CHAR, PA_WCHAR, PA_STRING, PA_WSTRING, PA_POINTER, PA_FLOAT, PA_DOUBLE, PA_LAST };

#define PA_FLAG_MASK 0xff00
#define PA_FLAG_LONG_LONG (1 << 8)
#define PA_FLAG_LONG_DOUBLE PA_FLAG_LONG_LONG
#define PA_FLAG_LONG (1 << 9)
#define PA_FLAG_SHORT (1 << 10)
#define PA_FLAG_PTR (1 << 11)

/* A run of digits followed by '$': its number (1-based), else 0 and *p
 * unmoved. */
static inline size_t __gexcc_pa_position(const char **p)
{
    const char *q = *p;
    size_t n = 0;
    while (*q >= '0' && *q <= '9')
        n = n * 10 + (size_t)(*q++ - '0');
    if (q == *p || *q != '$')
        return 0;
    *p = q + 1;
    return n;
}

static inline void __gexcc_pa_put(int *types, size_t n, size_t at, int type)
{
    if (at < n)
        types[at] = type;
}

/* The number of arguments FMT takes; the first N of their types go to
 * ARGTYPES. Positional arguments (%2$s) go where their numbers say. */
static inline size_t parse_printf_format(const char *fmt, size_t n, int *argtypes)
{
    size_t next = 0, max = 0;
    const char *p = fmt;
    while ((p = __builtin_strchr(p, '%'))) {
        p++;
        if (*p == '%') {
            p++;
            continue;
        }
        size_t pos = __gexcc_pa_position(&p);
        while (*p && __builtin_strchr("-+ #0'I", *p))
            p++;
        /* width */
        if (*p == '*') {
            p++;
            size_t wp = __gexcc_pa_position(&p);
            size_t at = wp ? wp - 1 : next++;
            __gexcc_pa_put(argtypes, n, at, PA_INT);
            if (at + 1 > max)
                max = at + 1;
        } else
            while (*p >= '0' && *p <= '9')
                p++;
        /* precision */
        if (*p == '.') {
            p++;
            if (*p == '*') {
                p++;
                size_t pp = __gexcc_pa_position(&p);
                size_t at = pp ? pp - 1 : next++;
                __gexcc_pa_put(argtypes, n, at, PA_INT);
                if (at + 1 > max)
                    max = at + 1;
            } else
                while (*p >= '0' && *p <= '9')
                    p++;
        }
        /* length */
        int flags = 0, hh = 0;
        for (;;) {
            if (*p == 'h') {
                if (flags & PA_FLAG_SHORT)
                    hh = 1;
                flags |= PA_FLAG_SHORT;
            } else if (*p == 'l')
                flags |= (flags & PA_FLAG_LONG) ? PA_FLAG_LONG_LONG : PA_FLAG_LONG;
            else if (*p == 'L' || *p == 'q' || *p == 'j')
                flags |= PA_FLAG_LONG_LONG;
            else if (*p == 'z' || *p == 'Z' || *p == 't')
                flags |= sizeof(size_t) == sizeof(long long) && sizeof(long) != sizeof(long long)
                    ? PA_FLAG_LONG_LONG : PA_FLAG_LONG;
            else
                break;
            p++;
        }
        if (flags & PA_FLAG_LONG_LONG)
            flags &= ~PA_FLAG_LONG;
        int type;
        switch (*p) {
        case 'd': case 'i': case 'o': case 'u': case 'x': case 'X':
            type = hh ? PA_CHAR : PA_INT | flags;
            break;
        case 'c': /* %lc too, as glibc has it */
            type = PA_CHAR;
            break;
        case 'C':
            type = PA_WCHAR;
            break;
        case 's': /* %ls too, as glibc has it */
            type = PA_STRING;
            break;
        case 'S':
            type = PA_WSTRING;
            break;
        case 'p':
            type = PA_POINTER;
            break;
        case 'n':
            type = PA_INT | PA_FLAG_PTR;
            break;
        case 'e': case 'E': case 'f': case 'F': case 'g': case 'G': case 'a': case 'A':
            type = PA_DOUBLE | (flags & PA_FLAG_LONG_LONG ? PA_FLAG_LONG_DOUBLE : 0);
            break;
        case 'm':
            p++;
            continue; /* strerror(errno): no argument */
        case '\0':
            return max;
        default:
            p++;
            continue;
        }
        p++;
        size_t at = pos ? pos - 1 : next++;
        __gexcc_pa_put(argtypes, n, at, type);
        if (at + 1 > max)
            max = at + 1;
    }
    return max;
}

#ifdef __cplusplus
}
#endif

#endif
