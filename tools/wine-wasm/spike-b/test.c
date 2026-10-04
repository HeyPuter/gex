/* W0(b): an i386 Windows program calling demo.dll, whose code is native
 * wasm (demo.c) behind a PE skeleton, through spec-generated adapters,
 * with callbacks back into this program's x86 code through the ICALL
 * check. Built with MinGW (i686). */
#include <windows.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

typedef int (WINAPI *add_t)(int, int);
typedef int64_t (WINAPI *mul64_t)(int64_t, int);
typedef double (__cdecl *scale_t)(double, int);
typedef int (__cdecl *cmp_t)(const void *, const void *);
typedef void (__cdecl *sort_t)(int *, int, cmp_t);
typedef int (WINAPI *apply_cb_t)(int);
typedef int (WINAPI *apply_t)(apply_cb_t, int);
typedef int (__cdecl *calls_t)(void);

static int guest_cmp_calls;
static int __cdecl guest_cmp(const void *a, const void *b)
{
    guest_cmp_calls++;
    return *(const int *)a - *(const int *)b;
}
static int __cdecl guest_cmp_desc(const void *a, const void *b) { return *(const int *)b - *(const int *)a; }
static int WINAPI guest_triple(int x) { return 3 * x; }
static __attribute__((noinline)) int WINAPI guest_add(int a, int b) { return a + b; }
static add_t volatile guest_add_ptr = guest_add;

static LARGE_INTEGER freq;
static double secs(LARGE_INTEGER a, LARGE_INTEGER b) { return (double)(b.QuadPart - a.QuadPart) / freq.QuadPart; }

int main(int argc, char **argv)
{
    QueryPerformanceFrequency(&freq);
    HMODULE h = LoadLibraryA("demo.dll");
    if (!h) { printf("LoadLibrary failed: %lu\n", GetLastError()); return 1; }
    add_t add = (add_t)GetProcAddress(h, "demo_add");
    mul64_t mul64 = (mul64_t)GetProcAddress(h, "demo_mul64");
    scale_t scale = (scale_t)GetProcAddress(h, "demo_scale");
    sort_t sort = (sort_t)GetProcAddress(h, "demo_sort");
    apply_t apply = (apply_t)GetProcAddress(h, "demo_apply");
    cmp_t ncmp = (cmp_t)GetProcAddress(h, "demo_native_cmp");
    calls_t calls = (calls_t)GetProcAddress(h, "demo_calls");
    printf("demo.dll at %p, demo_add at %p\n", (void *)h, (void *)add);

    int fails = 0;
#define CHECK(c) do { if (!(c)) { printf("FAIL %s\n", #c); fails++; } } while (0)
    int s = add(2, 40);
    printf("demo_add(2, 40) = %d\n", s); CHECK(s == 42);
    int64_t m = mul64(0x123456789ll, 16);
    printf("demo_mul64(0x123456789, 16) = %#llx\n", (unsigned long long)m); CHECK(m == 0x1234567890ll);
    double d = scale(1.5, 4);
    printf("demo_scale(1.5, 4) = %g\n", d); CHECK(d == 6.25);
    int a[8] = { 5, 3, 9, 1, 7, 2, 8, 6 };
    sort(a, 8, guest_cmp);
    printf("demo_sort with an x86 comparator: %d %d %d %d %d %d %d %d (%d callbacks)\n", a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], guest_cmp_calls);
    for (int i = 1; i < 8; i++) CHECK(a[i - 1] <= a[i]);
    sort(a, 8, guest_cmp_desc);
    CHECK(a[0] == 9 && a[7] == 1);
    sort(a, 8, ncmp);   /* a native export, by its guest address */
    printf("demo_sort with demo_native_cmp's address: %d .. %d\n", a[0], a[7]); CHECK(a[0] == 1 && a[7] == 9);
    int r = apply(guest_triple, 7);
    printf("demo_apply(x86 stdcall callback, 7) = %d\n", r); CHECK(r == 42);
    printf("calls inside demo.dll: %d\n", calls());

    /* timings, natively under gexc (the browser numbers are W0(c)'s) */
    int n = argc > 1 ? atoi(argv[1]) : 1000000;
    LARGE_INTEGER t0, t1;
    QueryPerformanceCounter(&t0);
    for (int i = 0; i < n; i++) s = add(s, i);
    QueryPerformanceCounter(&t1);
    printf("x86 -> native (stdcall adapter): %.1f ns/call\n", secs(t0, t1) * 1e9 / n);
    add_t gadd = guest_add_ptr;
    QueryPerformanceCounter(&t0);
    for (int i = 0; i < n; i++) s = gadd(s, i);
    QueryPerformanceCounter(&t1);
    printf("x86 -> x86, same loop through a pointer (baseline): %.1f ns/call\n", secs(t0, t1) * 1e9 / n);
    int big[512];
    for (int i = 0; i < 512; i++) big[i] = (i * 7919) % 512;
    guest_cmp_calls = 0;
    QueryPerformanceCounter(&t0);
    sort(big, 512, guest_cmp);
    QueryPerformanceCounter(&t1);
    printf("native -> x86 (exit stub): %.1f ns/callback over %d callbacks\n", secs(t0, t1) * 1e9 / guest_cmp_calls, guest_cmp_calls);
    guest_cmp_calls = 0;
    for (int i = 0; i < 512; i++) big[i] = (i * 7919) % 512;
    QueryPerformanceCounter(&t0);
    qsort(big, 512, sizeof(int), guest_cmp);
    QueryPerformanceCounter(&t1);
    printf("msvcrt qsort (x86 Wine) with the same comparator (baseline): %.1f ns/callback over %d callbacks\n", secs(t0, t1) * 1e9 / guest_cmp_calls, guest_cmp_calls);
    printf(fails ? "FAILED\n" : "PASS\n");
    return fails;
}
