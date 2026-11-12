/* Guest-to-native call cost: guest x86-64 code calling convdll.c's native
 * functions N times (argv[1], default 2000000) through their table indices
 * (the native module's entry path) and through an export, with guest-to-guest
 * and native-to-native indirect calls as references. One line per kind:
 * "bench KIND NS ns/call". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include "conv.h"

static LARGE_INTEGER freq;
static double now_ns(void)
{
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    return (double)c.QuadPart * 1e9 / (double)freq.QuadPart;
}

static __attribute__((noinline)) int guest_i4(int a, int b, int c, int d) { return a + 2 * b + 3 * c + 4 * d; }

#define LOOP(kind, expr) do { \
    volatile long long sink = 0; double t0 = now_ns(); \
    for (int i = 0; i < n; i++) sink += (expr); \
    printf("bench %s %.1f ns/call\n", kind, (now_ns() - t0) / n); \
    if (!sink && n > 1) printf("(sink %lld)\n", (long long)sink); \
} while (0)

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 2000000;
    HMODULE m = LoadLibraryA("convdll.dll");
    const struct conv_table *(*get)(void);
    int (*selfloop)(int);
    int (*e_i4)(int, int, int, int);
    int (*volatile g_i4)(int, int, int, int) = guest_i4;
    const struct conv_table *volatile t;

    QueryPerformanceFrequency(&freq);
    if (!m || !(get = (void *)GetProcAddress(m, "conv_table")) || !(selfloop = (void *)GetProcAddress(m, "conv_selfloop"))
        || !(e_i4 = (void *)GetProcAddress(m, "c_i4")))
    {
        printf("FAIL convdll.dll: %lu\n1 failures\n", GetLastError());
        return 1;
    }
    t = get();
    /* a first call of each, outside the timing (translation, binding) */
    t->c_i0(); t->c_i4(1, 2, 3, 4); t->c_i16(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16); t->c_d(1, 2); t->c_r8(1); e_i4(1, 2, 3, 4); selfloop(10);
    LOOP("guest->guest i4", g_i4(i, 1, 2, 3));
    LOOP("guest->native i0", t->c_i0());
    LOOP("guest->native i4", t->c_i4(i, 1, 2, 3));
    LOOP("guest->native i16", t->c_i16(i, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
    LOOP("guest->native d", (long long)t->c_d(i, 2.0));
    LOOP("guest->native r8", t->c_r8(i).a);
    LOOP("guest->export i4", e_i4(i, 1, 2, 3));
    {
        double t0 = now_ns();
        int s = selfloop(n);
        printf("bench native->native i4 %.1f ns/call\n", (now_ns() - t0) / n);
        if (!s && n > 1) printf("(sink %d)\n", s);
    }
    printf("ok icallbench %d calls each\n0 failures\n", n);
    fflush(stdout);
    return 0;
}
