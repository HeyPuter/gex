/* ELF providers' conformance: glibc's string printf family (snprintf,
 * vsnprintf, sprintf, vsprintf). Every conversion and flag combination
 * over a grid of values, truncation at every buffer size, and the cases
 * the provider hands back to glibc (positional arguments, %m, wide
 * characters, %a, long double). Prints each result and return value. */
#define _GNU_SOURCE
#include <errno.h>
#include <float.h>
#include <limits.h>
#include <math.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>

static char buf[4096];
static void show(const char *what, int r) { printf("%s -> %d [%s]\n", what, r, buf); }

static int vs(char *b, size_t n, const char *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vsnprintf(b, n, f, ap);
    va_end(ap);
    return r;
}
static int vsp(char *b, const char *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vsprintf(b, f, ap);
    va_end(ap);
    return r;
}

int main(void) {
    static const char *iflags[] = {"", "-", "+", " ", "#", "0", "-+", "+0", " 0", "#0", "-#", "-0"};
    static const char *iwidth[] = {"", "1", "5", "12", "*"};
    static const char *iprec[] = {"", ".0", ".1", ".3", ".10", ".*"};
    static const char *iconv[] = {"d", "i", "u", "x", "X", "o", "hhd", "hd", "ld", "lld", "zu", "jd", "hhx", "lx", "td"};
    static const long long ivals[] = {0, 1, -1, 7, -42, 255, 256, 65535, -32768, 2147483647, -2147483647 - 1, 4294967295LL, LLONG_MAX, LLONG_MIN, 0x123456789abcLL};
    char f[64];
    for (unsigned a = 0; a < sizeof iflags / sizeof *iflags; ++a)
        for (unsigned b = 0; b < sizeof iwidth / sizeof *iwidth; ++b)
            for (unsigned c = 0; c < sizeof iprec / sizeof *iprec; ++c)
                for (unsigned d = 0; d < sizeof iconv / sizeof *iconv; ++d) {
                    snprintf(f, sizeof f, "<%%%s%s%s%s>", iflags[a], iwidth[b], iprec[c], iconv[d]);
                    for (unsigned v = 0; v < sizeof ivals / sizeof *ivals; ++v) {
                        int r;
                        long long x = ivals[v];
                        if (b == 4 && c == 5) r = snprintf(buf, sizeof buf, f, 7, 3, x);
                        else if (b == 4 || c == 5) r = snprintf(buf, sizeof buf, f, 7 - 10 * (int)(v & 1), x);
                        else r = snprintf(buf, sizeof buf, f, x);
                        printf("%s %lld -> %d [%s]\n", f, x, r, buf);
                    }
                }
    static const char *fflags[] = {"", "-", "+", " ", "#", "0", "+0", "#0", "-0"};
    static const char *fwidth[] = {"", "8", "20"};
    static const char *fprec[] = {"", ".0", ".1", ".3", ".8", ".17", ".40"};
    static const char *fconv[] = {"f", "F", "e", "E", "g", "G"};
    static const double fvals[] = {0.0, -0.0, 1.0, -1.5, 0.1, 2.5, 3.5, 0.5, 1e-5, 123456.789, 1e15, 1e16, 1e22, 1e300, 5e-324, DBL_MAX, DBL_MIN,
                                   1.0 / 3, 2.0 / 3, 999999.5, 0.00001234, 9.9999999, 0.05, 0.15, 0.25, 0.35, 1e-310, 3.14159265358979,
                                   INFINITY, -INFINITY, NAN, -NAN};
    for (unsigned a = 0; a < sizeof fflags / sizeof *fflags; ++a)
        for (unsigned b = 0; b < sizeof fwidth / sizeof *fwidth; ++b)
            for (unsigned c = 0; c < sizeof fprec / sizeof *fprec; ++c)
                for (unsigned d = 0; d < sizeof fconv / sizeof *fconv; ++d) {
                    snprintf(f, sizeof f, "<%%%s%s%s%s>", fflags[a], fwidth[b], fprec[c], fconv[d]);
                    for (unsigned v = 0; v < sizeof fvals / sizeof *fvals; ++v) {
                        int r = snprintf(buf, sizeof buf, f, fvals[v]);
                        printf("%s %a -> %d [%s]\n", f, fvals[v], r, buf);
                    }
                }
    /* Strings, characters, pointers, %n, %%. */
    show("s", snprintf(buf, sizeof buf, "<%s|%10s|%-10s|%.2s|%10.3s|%s>", "hello", "hi", "left", "abcdef", "abcdef", ""));
    show("null s", snprintf(buf, sizeof buf, "<%s|%.3s|%.6s|%10s|%-8s>", (char *)0, (char *)0, (char *)0, (char *)0, (char *)0));
    show("c", snprintf(buf, sizeof buf, "<%c|%5c|%-5c|%05c>", 'a', 'b', 'c', 'd'));
    show("p", snprintf(buf, sizeof buf, "<%p|%p|%20p|%-20p|%020p|%+p|% p>", (void *)0, (void *)buf, (void *)0x1234, (void *)0x1234, (void *)0x1234, (void *)0x1234, (void *)0x1234));
    {
        int n1 = 0;
        short n2 = 0;
        long n3 = 0;
        signed char n4 = 0;
        show("n", snprintf(buf, sizeof buf, "abc%ndef%hnghi%lnjk%hhn|%%|%5%|", &n1, &n2, &n3, &n4));
        printf("n: %d %d %ld %d\n", n1, n2, n3, n4);
    }
    /* Many arguments (past the registers), mixed. */
    show("many", snprintf(buf, sizeof buf, "%d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %s %c", 1, 1.5, 2, 2.5, 3, 3.5, 4, 4.5, 5, 5.5, 6, 6.5, 7, 7.5, 8, 8.5, 9,
                                 9.5, 10, 10.5, "end", 'Z'));
    show("vs", vs(buf, sizeof buf, "%d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %d %f %s", 1, 1.5, 2, 2.5, 3, 3.5, 4, 4.5, 5, 5.5, 6, 6.5, 7, 7.5, 8, 8.5, 9, 9.5,
                    10, 10.5, "end"));
    show("sprintf", sprintf(buf, "%s=%08.3f %x %lld", "v", -3.25, 0xbeef, -12345678901LL));
    show("vsprintf", vsp(buf, "%s=%08.3f %x %lld %g", "v", -3.25, 0xbeef, -12345678901LL, 1e-7));
    /* Truncation at every size. */
    for (int n = 0; n < 24; ++n) {
        memset(buf, '#', 32);
        int r = snprintf(buf, n, "%s-%d-%.2f", "trunc", 12345, 6.789);
        printf("n=%d -> %d [%.32s]\n", n, r, buf);
        memset(buf, '#', 32);
        r = vs(buf, n, "%s-%d-%.2f", "trunc", 12345, 6.789);
        printf("vn=%d -> %d [%.32s]\n", n, r, buf);
    }
    show("NULL buf", snprintf((char *)0, 0, "%d%s", 42, "abc"));
    /* What the provider hands to glibc. */
    show("positional", snprintf(buf, sizeof buf, "%2$s %1$d %2$s", 7, "x"));
    errno = ENOENT;
    show("m", snprintf(buf, sizeof buf, "<%m>"));
    show("ls", snprintf(buf, sizeof buf, "<%ls|%lc>", L"wide", (wint_t)L'w'));
    show("a", snprintf(buf, sizeof buf, "<%a|%A|%.2a>", 1.5, -0.1, 3.0));
    show("Lf", snprintf(buf, sizeof buf, "<%Lf|%.20Le|%Lg>", 1.5L, 1.0L / 3, 1e-300L * 1e-300L));
    show("vm", vs(buf, sizeof buf, "<%m|%1$d>", 5));
    return 0;
}
