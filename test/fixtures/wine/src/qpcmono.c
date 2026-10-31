/* The performance counter as one timeline (H, the Unix-side QPC fast path):
 * QueryPerformanceCounter's reads from the PE side (gex.clock_ns, and every
 * 64th call Wine's Unix NtQueryPerformanceCounter) never go backwards, and a
 * wait the Unix side times out, a Sleep and the tick count agree with it.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>

static int failures;

static void check(int ok, const char *what, double got)
{
    if (ok) printf("ok %s (%.1f)\n", what, got);
    else { printf("FAIL %s (%.1f)\n", what, got); failures++; }
}

static double ms_since(LARGE_INTEGER a, LARGE_INTEGER f)
{
    LARGE_INTEGER b;
    QueryPerformanceCounter(&b);
    return (b.QuadPart - a.QuadPart) * 1000.0 / f.QuadPart;
}

int main(void)
{
    LARGE_INTEGER f, prev, now, t0;
    int back = 0, i;
    HANDLE ev = CreateEventA(NULL, TRUE, FALSE, NULL);
    ULONGLONG tick0;
    double qpc_ms, tick_ms;

    QueryPerformanceFrequency(&f);
    check(f.QuadPart == 10000000, "frequency is 10 MHz", (double)f.QuadPart);
    QueryPerformanceCounter(&prev);
    for (i = 0; i < 2000000; i++)
    {
        QueryPerformanceCounter(&now);
        if (now.QuadPart < prev.QuadPart) back++;
        prev = now;
    }
    check(!back, "2000000 reads never go backwards", back);

    QueryPerformanceCounter(&t0);
    tick0 = GetTickCount64();
    check(WaitForSingleObject(ev, 50) == WAIT_TIMEOUT, "a 50 ms wait times out", 0);
    qpc_ms = ms_since(t0, f);
    check(qpc_ms >= 45 && qpc_ms < 2000, "the 50 ms wait lasts 45 ms or more by the counter", qpc_ms);

    QueryPerformanceCounter(&t0);
    Sleep(30);
    qpc_ms = ms_since(t0, f);
    check(qpc_ms >= 25 && qpc_ms < 2000, "Sleep(30) lasts 25 ms or more by the counter", qpc_ms);

    QueryPerformanceCounter(&t0);
    tick0 = GetTickCount64();
    Sleep(200);
    qpc_ms = ms_since(t0, f);
    tick_ms = (double)(GetTickCount64() - tick0);
    check(tick_ms - qpc_ms < 60 && qpc_ms - tick_ms < 60, "the tick count keeps to the counter over 200 ms", tick_ms - qpc_ms);

    printf("%d failures\n", failures);
    return failures != 0;
}
