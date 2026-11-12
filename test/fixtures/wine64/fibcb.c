/* Fibers switched inside guest callbacks of a native function (convdll's
 * c_call, which calls its callback and adds 1). Each fiber has a stack of
 * its own, so a callback's call-out stays open while another fiber runs on
 * another stack, above or below it:
 * 1. a callback switches to a fiber and back, N times;
 * 3. fiber A's callback switches to fiber B, whose own callback switches
 *    back to A; A switches to B again, B's call returns and B switches
 *    back, and A's call returns (nested across fibers), N times.
 * With "interleaved": A's call returns while B's is still open, then B's
 * (not last in, first out; native code may refuse it, but by ending the
 * process with a message, never by hanging).
 * Prints a line per case and "N failures"; argv[1] is N (2000). */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "conv.h"

static const struct conv_table *t;
static LPVOID fa, fb;
static volatile int stage, done_b, rb;

static int cb_s(int x) { SwitchToFiber(fb); return x * 3; }
static void WINAPI fiber_s(void *p) { for (;;) { done_b++; SwitchToFiber(fa); } }

static int cb_b3(int x) { stage = 2; SwitchToFiber(fa); stage = 4; return x * 5; }
static void WINAPI fiber_b3(void *p) { rb = t->c_call(cb_b3, 7); done_b = 1; SwitchToFiber(fa); }
static int cb_a3(int x)
{
    stage = 1;
    SwitchToFiber(fb); /* B is in cb_b3 */
    SwitchToFiber(fb); /* B's call returned */
    return x * 3;
}

static int cb_b(int x) { stage = 2; SwitchToFiber(fa); stage = 4; return x * 5; }
static int cb_a(int x) { stage = 1; SwitchToFiber(fb); stage = 3; return x * 3; }
static void WINAPI fiber_b(void *p) { rb = t->c_call(cb_b, 7); done_b = 1; SwitchToFiber(fa); }

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 2000, ok, r, failures = 0;
    HMODULE m = LoadLibraryA("convdll.dll");
    t = ((const struct conv_table *(*)(void))GetProcAddress(m, "conv_table"))();
    fa = ConvertThreadToFiber(NULL);
    if (argc > 2 && !strcmp(argv[2], "interleaved"))
    {
        ok = 0;
        for (int i = 0; i < n; i++)
        {
            done_b = 0; stage = 0;
            fb = CreateFiber(0, fiber_b, NULL);
            r = t->c_call(cb_a, 5);
            if (r == 16 && stage == 3) { SwitchToFiber(fb); ok += done_b && rb == 36 && stage == 4; }
            DeleteFiber(fb);
        }
        printf("%s interleaved (%d of %d)\n", ok == n ? "ok" : "FAIL", ok, n);
        printf("%d failures\n", ok != n);
        return 0;
    }

    ok = 0;
    fb = CreateFiber(0, fiber_s, NULL);
    for (int i = 0; i < n; i++) ok += t->c_call(cb_s, i) == i * 3 + 1;
    failures += ok != n || done_b != n;
    printf("%s a switch to a fiber and back inside a callback (%d of %d, %d switches)\n", ok == n && done_b == n ? "ok" : "FAIL", ok, n, done_b);
    fflush(stdout);
    DeleteFiber(fb);

    ok = 0;
    for (int i = 0; i < n; i++)
    {
        done_b = 0; stage = 0;
        fb = CreateFiber(0, fiber_b3, NULL);
        r = t->c_call(cb_a3, 5);
        ok += r == 16 && done_b && rb == 36 && stage == 4;
        DeleteFiber(fb);
    }
    failures += ok != n;
    printf("%s callbacks nested across fibers (%d of %d)\n", ok == n ? "ok" : "FAIL", ok, n);
    printf("%d failures\n", failures);
    return 0;
}
