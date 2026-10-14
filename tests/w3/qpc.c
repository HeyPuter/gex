/* The performance counter on W3 (tests/w3/qpc.sh). PE ntdll answers
 * NtQueryPerformanceCounter from gex.clock_ns with no system call
 * (docs/wine-overhead.md F2); this checks that what it answers is the Unix
 * side's counter:
 *   - the frequency is 10 MHz (QueryPerformanceFrequency and
 *     NtQueryPerformanceCounter's second argument), as the Unix side's;
 *   - QPC never goes backwards in a thread, and orders across threads: a
 *     value read after an event another thread set after its own read is
 *     not smaller (ping-pong between two threads, ROUNDS times);
 *   - QPC deltas agree with the system time's (NtQuerySystemTime: a system
 *     call, the Unix side's clock_gettime( CLOCK_REALTIME ), the clock the
 *     monotonic one is made from) to within 2 ms over each of several
 *     100 ms sleeps, in every thread.
 * Prints "qpc ok THREADS ROUNDS". */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <stdlib.h>

NTSTATUS WINAPI NtQueryPerformanceCounter(LARGE_INTEGER *, LARGE_INTEGER *);
NTSTATUS WINAPI NtQuerySystemTime(LARGE_INTEGER *);

static int rounds = 20000;
static volatile LONG bad;
static HANDLE ping, pong;
static volatile LONGLONG shared;

#define FAIL(...) do { if (InterlockedIncrement(&bad) < 8) printf(__VA_ARGS__); } while (0)

static LONGLONG qpc(int nt)
{
    LARGE_INTEGER v, f;
    if (nt) { f.QuadPart = 0; NtQueryPerformanceCounter(&v, &f); if (f.QuadPart != 10000000) FAIL("NtQueryPerformanceCounter frequency %lld\n", f.QuadPart); }
    else QueryPerformanceCounter(&v);
    return v.QuadPart;
}

static DWORD WINAPI agree(void *arg)
{
    int i;
    for (i = 0; i < 3; i++) {
        LARGE_INTEGER s0, s1;
        LONGLONG q0, q1, dq, ds;
        NtQuerySystemTime(&s0); q0 = qpc(i & 1);
        Sleep(100);
        q1 = qpc(!(i & 1)); NtQuerySystemTime(&s1);
        dq = q1 - q0; ds = s1.QuadPart - s0.QuadPart;   /* both 100 ns units */
        if (dq < 900000 || ds - dq > 20000 || dq - ds > 20000)
            FAIL("thread %lu: QPC delta %lld, system time delta %lld (100 ns)\n", GetCurrentThreadId(), dq, ds);
    }
    return 0;
}

static DWORD WINAPI ponger(void *arg)
{
    int i;
    for (i = 0; i < rounds; i++) {
        WaitForSingleObject(ping, INFINITE);
        LONGLONG v = qpc(i & 1);
        if (v < shared) FAIL("round %d: QPC %lld after the other thread's %lld\n", i, v, shared);
        shared = qpc(0);
        SetEvent(pong);
    }
    return 0;
}

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 4, i;
    HANDLE t[64], p;
    LARGE_INTEGER f;
    LONGLONG last = 0;

    if (argc > 2) rounds = atoi(argv[2]);
    if (n > 63) n = 63;
    if (!QueryPerformanceFrequency(&f) || f.QuadPart != 10000000) { printf("QueryPerformanceFrequency %lld\n", f.QuadPart); return 1; }
    for (i = 0; i < 100000; i++) {
        LONGLONG v = qpc(i & 1);
        if (v < last) FAIL("QPC went back: %lld after %lld\n", v, last);
        last = v;
    }
    for (i = 0; i < n; i++) t[i] = CreateThread(NULL, 0, agree, NULL, 0, NULL);
    agree(NULL);
    WaitForMultipleObjects(n, t, TRUE, INFINITE);
    for (i = 0; i < n; i++) CloseHandle(t[i]);
    ping = CreateEventA(NULL, FALSE, FALSE, NULL);
    pong = CreateEventA(NULL, FALSE, FALSE, NULL);
    p = CreateThread(NULL, 0, ponger, NULL, 0, NULL);
    for (i = 0; i < rounds; i++) {
        shared = qpc(i & 1);
        SetEvent(ping);
        WaitForSingleObject(pong, INFINITE);
        if (qpc(0) < shared) FAIL("round %d: main's QPC before the other thread's %lld\n", i, shared);
    }
    WaitForSingleObject(p, INFINITE);
    if (bad) { printf("%ld failures\n", bad); return 1; }
    printf("qpc ok %d %d\n", n + 1, rounds);
    return 0;
}
