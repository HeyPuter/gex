/* W64.2: an x64 program calling abi.dll (abi.c as a wasm64 module behind a
 * winebuild skeleton) through every kind of Win64 entry adapter.
 *   x86_64-w64-mingw32-gcc -O1 -o abi_test.exe abi_test.c */
#include <windows.h>
#include <stdarg.h>
#include <stdio.h>

typedef struct { int x, y; } pair;
typedef struct { int a, b, c; } triple;
typedef struct { short lo, hi; } small;
static int cb(int x) { return x * 3; }
static double cb6(int a, double b, int c, float d, long long e, int f)
{
    return a * 100000.0 + b * 10000 + c * 1000 + d * 100 + e * 10 + f;
}
static pair cbp(int k) { pair p = { k, k + 1 }; return p; }

static int (*vpv)(const char *, va_list);
static int call_v(const char *fmt, ...)
{
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = vpv(fmt, ap);
    va_end(ap);
    return r;
}

int main(void)
{
    HMODULE h = LoadLibraryA("abi.dll");
    int fails = 0;
#define GET(t, n) __typeof__(t) n = (__typeof__(t))GetProcAddress(h, #n)
#define CHECK(c) do { int ok_ = (c); printf("%-4s %s\n", ok_ ? "ok" : "FAIL", #c); fails += !ok_; } while (0)
    if (!h) { printf("LoadLibrary failed %lu\n", GetLastError()); return 1; }
    GET(int (*)(int, int), abi_add);
    GET(long long (*)(long long, int), abi_mul64);
    GET(double (*)(double, float, double), abi_fma);
    GET(float (*)(float), abi_half);
    GET(double (*)(int, double, int, float, long long, double, int *), abi_mix);
    GET(int (*)(LARGE_INTEGER), abi_large);
    GET(int (*)(small), abi_small);
    GET(int (*)(triple, int), abi_big);
    GET(pair (*)(int, int), abi_pair);
    GET(triple (*)(int), abi_triple);
    GET(char *(*)(char *, int), abi_ptr);
    GET(void (*)(int *), abi_void);
    GET(int (*)(int (*)(int), int), abi_callback);
    GET(double (*)(double (*)(int, double, int, float, long long, int), int), abi_callback6);
    GET(int (*)(pair (*)(int)), abi_callback_pair);
    GET(int *, abi_data);
    GET(DWORD (*)(void), abi_forward);
    int v = 0, g = 0;
    LARGE_INTEGER li;
    small sm = { 3, 4 };
    triple t = { 1, 2, 3 };
    char buf[8];
    li.u.LowPart = 5; li.u.HighPart = 7;
    CHECK(abi_add(40, 2) == 42);
    CHECK(abi_mul64(0x100000000ll, 3) == 0x300000000ll);
    CHECK(abi_fma(1.5, 2.0f, 0.25) == 3.25);
    CHECK(abi_half(5.0f) == 2.5f);
    CHECK(abi_mix(1, 2.0, 3, 4.0f, 5, 6.0, &g) == 123456.0 && g == 9);
    CHECK(abi_large(li) == 7005);
    CHECK(abi_small(sm) == 403);
    CHECK(abi_big(t, 5) == 325 && t.a == 1);
    pair p = abi_pair(10, 20);
    CHECK(p.x == 11 && p.y == 22);
    triple u = abi_triple(4);
    CHECK(u.a == 4 && u.b == 8 && u.c == 12);
    CHECK(abi_ptr(buf, 3) == buf + 3);
    abi_void(&v);
    CHECK(v == 77);
    CHECK(abi_callback(cb, 5) == 16);
    CHECK(abi_callback6(cb6, 1) == 107059.0);
    CHECK(abi_callback_pair(cbp) == 56);
    CHECK(abi_data && *abi_data == 1234);
    CHECK(abi_forward != NULL && abi_forward() + 1000 > GetTickCount());
    CHECK(GetProcAddress(h, "abi_missing") != NULL);
    int (*noname)(int) = (void *)GetProcAddress(h, (LPCSTR)100);
    CHECK(noname && noname(5) == 105);
    int (*vp)(const char *, ...) = (void *)GetProcAddress(h, "abi_printf");
    long long (*vq)(int, ...) = (void *)GetProcAddress(h, "abi_quads");
    CHECK(vp && vp("%d %f %I64d %f %s", 1, 2.5, 3ll, 0.25, "2") == 87);
    CHECK(vq && vq(4, 1ll, 2ll, 3ll, 4ll) == 1234);
    vpv = (void *)GetProcAddress(h, "abi_vprintf");
    CHECK(vpv && call_v("%d %f %I64d %f %s", 1, 2.5, 3ll, 0.25, "2") == 87);
    printf(fails ? "FAILED\n" : "PASS\n");
    return fails;
}
