/* <printf.h>'s parse_printf_format (systemd's format checks): what each
 * argument of a format is, by the size va_arg takes it at, on the host
 * (glibc) and in gex (gexcc's printf.h). */
#include <printf.h>
#include <stdint.h>
#include <stdio.h>
#include <stddef.h>

static size_t size_of(int t) {
    int b = t & ~PA_FLAG_MASK, f = t & PA_FLAG_MASK;
    if (f & PA_FLAG_PTR)
        return sizeof(void *);
    switch (b) {
    case PA_INT:
        return f & PA_FLAG_LONG_LONG ? sizeof(long long) : f & PA_FLAG_LONG ? sizeof(long) : sizeof(int);
    case PA_CHAR: case PA_WCHAR:
        return sizeof(int);
    case PA_STRING: case PA_WSTRING: case PA_POINTER:
        return sizeof(void *);
    case PA_DOUBLE:
        return f & PA_FLAG_LONG_DOUBLE ? sizeof(long double) : sizeof(double);
    }
    return 0;
}

static void check(const char *fmt, size_t want_n, const size_t *want) {
    int t[16];
    size_t n = parse_printf_format(fmt, 16, t), bad = 0;
    for (size_t i = 0; i < n && i < want_n; i++)
        bad += size_of(t[i]) != want[i];
    printf("%-30s %s\n", fmt, n == want_n && !bad ? "ok" : "MISMATCH");
}

#define CHECK(fmt, ...) do { size_t w[] = { 0, __VA_ARGS__ }; check(fmt, sizeof w / sizeof *w - 1, w + 1); } while (0)

int main(void) {
    int t[4];
    printf("plain: %zu\n", parse_printf_format("plain %%", 4, t));
    CHECK("%d %i %u %x %o", sizeof(int), sizeof(int), sizeof(int), sizeof(int), sizeof(int));
    CHECK("%hd %hhd %ld %lld", sizeof(int), sizeof(int), sizeof(long), sizeof(long long));
    CHECK("%jd %zu %td", sizeof(intmax_t), sizeof(size_t), sizeof(ptrdiff_t));
    CHECK("%s %ls %c %lc %p %n", sizeof(char *), sizeof(char *), sizeof(int), sizeof(int), sizeof(void *), sizeof(int *));
    CHECK("%f %Lf %e %g %a", sizeof(double), sizeof(long double), sizeof(double), sizeof(double), sizeof(double));
    CHECK("%*d %.*s %-*.*f", sizeof(int), sizeof(int), sizeof(int), sizeof(char *), sizeof(int), sizeof(int), sizeof(double));
    CHECK("%m %s", sizeof(char *));
    CHECK("%2$s %1$d", sizeof(int), sizeof(char *));
    CHECK("%-08.3lx|%+5d|% d|%#o", sizeof(long), sizeof(int), sizeof(int), sizeof(int));
    return 0;
}
