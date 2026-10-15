/* W64.2: one function per kind of Win64 entry adapter (abi.spec), compiled
 * as Wine's PE side is on wasm64 (wcc64 -D_WIN32: LLP64, the ICALL pass). */
typedef struct { int x, y; } pair;                   /* 8 bytes: in a register */
typedef struct { int a, b, c; } triple;              /* 12 bytes: by reference */
typedef struct { short lo, hi; } small;              /* 4 bytes */
typedef union { struct { unsigned int lo; int hi; } u; long long q; } large;

int abi_add(int a, int b) { return a + b; }
long long abi_mul64(long long a, int b) { return a * b; }
double abi_fma(double a, float b, double c) { return a * b + c; }
float abi_half(float x) { return x / 2; }
/* registers and stack, integer and floating point by position */
double abi_mix(int a, double b, int c, float d, long long e, double f, int *g)
{
    *g = 9;
    return a * 100000.0 + b * 10000 + c * 1000 + d * 100 + e * 10 + f;
}
int abi_large(large v) { return v.u.hi * 1000 + (int)v.u.lo; }
int abi_small(small s) { return s.hi * 100 + s.lo; }
int abi_big(triple t, int k) { t.a *= k; return t.a + t.b * 10 + t.c * 100; }
pair abi_pair(int x, int y) { pair p = { x + 1, y + 2 }; return p; }
triple abi_triple(int k) { triple t = { k, k * 2, k * 3 }; return t; }
char *abi_ptr(char *p, int k) { return p + k; }
void abi_void(int *out) { *out = 77; }
/* callbacks into the guest (the ICALL pass's exit stubs) */
int abi_callback(int (*f)(int), int x) { return f(x) + 1; }
double abi_callback6(double (*f)(int, double, int, float, long long, int), int x)
{
    return f(x, 0.5, 2, 0.25f, 3, 4);
}
int abi_callback_pair(pair (*f)(int)) { pair p = f(5); return p.x * 10 + p.y; }
int abi_data = 1234;

/* winecrt0's, for the stub: here it records the call. */
int abi_stub_called;
void __wine_spec_unimplemented_stub(const char *module, const char *function) { abi_stub_called = 1; }
int abi_noname(int x) { return x + 100; }

/* Varargs: a printf by name, whose format the adapter lays out again from
 * Win64's 8-byte slots, and a function of 8-byte arguments (the slots
 * passed as they are). */
#include <stdarg.h>
int abi_vprintf(const char *fmt, va_list ap)
{
    double sum = 0;
    for (; *fmt; fmt++)
    {
        if (*fmt != '%') continue;
        fmt++;
        if (fmt[0] == 'I' && fmt[1] == '6' && fmt[2] == '4') { fmt += 3; sum += va_arg(ap, long long); }
        else if (*fmt == 'f') sum += va_arg(ap, double);
        else if (*fmt == 'd') sum += va_arg(ap, int);
        else if (*fmt == 's') sum += *va_arg(ap, const char *) - '0';
    }
    return (int)(sum * 10);
}
int abi_printf(const char *fmt, ...)
{
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = abi_vprintf(fmt, ap);
    va_end(ap);
    return r;
}
long long abi_quads(int n, ...)
{
    va_list ap;
    long long sum = 0;
    int i;
    va_start(ap, n);
    for (i = 0; i < n; i++) sum = sum * 10 + va_arg(ap, long long);
    va_end(ap);
    return sum;
}
