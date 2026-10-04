/* W0(b): a small "Wine PE DLL" compiled to wasm32 instead of i386. Plain
 * C with Windows conventions in the source (__stdcall is a no-op on
 * wasm32); the spec file says how i386 callers call each export.
 *
 * Function pointers that may come from the guest are called through ICALL,
 * which is what the LLVM pass will insert at every indirect call site
 * (docs/wine-native.md §3.3, the select form decided after W0(c)). */
#include <stdint.h>
#include "icall.h"

#define WINAPI __stdcall

static int calls;   /* data: the module's own, at its __memory_base */

int WINAPI demo_add(int a, int b) { calls++; return a + b; }

int64_t WINAPI demo_mul64(int64_t a, int b) { calls++; return a * b; }

double demo_scale(double x, int k) { calls++; return x * k + 0.25; }

typedef int (*cmp_fn)(const void *, const void *);

/* An insertion sort with the caller's comparator: a callback into x86
 * when the comparator is guest code. */
void demo_sort(int *a, int n, cmp_fn cmp)
{
    calls++;
    for (int i = 1; i < n; i++) {
        int v = a[i], j = i - 1;
        while (j >= 0 && ICALL(cmp)(&a[j], &v) > 0) {
            a[j + 1] = a[j];
            j--;
        }
        a[j + 1] = v;
    }
}

typedef int (WINAPI *apply_fn)(int);

int WINAPI demo_apply(apply_fn f, int x) { calls++; return ICALL(f)(x) * 2; }

int demo_native_cmp(const int *a, const int *b) { return *a - *b; }

int demo_calls(void) { return calls; }

/* A double callback with a 64-bit argument: ST0 back from x86, and an
 * argument that is two stack words. */
typedef double (*fd_fn)(double, int64_t);

double demo_sum_fd(fd_fn f, int n)
{
    double s = 0;
    calls++;
    for (int i = 1; i <= n; i++) s += ICALL(f)(i * 0.5, (int64_t)i << 33);
    return s;
}
