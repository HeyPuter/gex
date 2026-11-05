/* msvcp120's _Thrd_current on 32-bit Wine (test/wow64/thrd): in N threads and the
 * main one, M calls each. The id half must be GetCurrentThreadId; the
 * handle half is a handle that is already closed (Wine's thread_current) or
 * 0 (gex's msvcp120_thrd provider), and the process's handle count must not
 * grow. Prints "thrd ok THREADS CALLS zero=Z" (Z: calls that returned
 * handle 0, all of them when the provider is bound), after "msvcp120 at
 * BASE" (where its unit must be built for the provider to bind). With a
 * third argument T, also times T more calls on the main thread and prints
 * "thrd ns PER-CALL" (a microbenchmark; time it only on a bench box). */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

typedef unsigned long long (__cdecl *thrd_current_fn)(void);
static thrd_current_fn thrd_current;
static int calls = 10000;
static volatile LONG bad, zero;

static DWORD WINAPI run(void *arg)
{
    DWORD me = GetCurrentThreadId();
    int i;
    for (i = 0; i < calls; i++) {
        unsigned long long r = thrd_current();
        if ((DWORD)(r >> 32) != me) {
            if (InterlockedIncrement(&bad) < 5)
                printf("thread %lu: _Thrd_current id %lu\n", me, (DWORD)(r >> 32));
        }
        if (!(DWORD)r) InterlockedIncrement(&zero);
    }
    return 0;
}

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 4, i;
    HANDLE t[64];
    DWORD before, after;
    HMODULE m;

    if (argc > 2) calls = atoi(argv[2]);
    if (n > 64) n = 64;
    if (!(m = LoadLibraryA("msvcp120.dll"))) { printf("no msvcp120.dll (%lu)\n", GetLastError()); return 1; }
    if (!(thrd_current = (thrd_current_fn)GetProcAddress(m, "_Thrd_current"))) { printf("no _Thrd_current\n"); return 1; }
    GetProcessHandleCount(GetCurrentProcess(), &before);
    for (i = 0; i < n; i++) t[i] = CreateThread(NULL, 0, run, NULL, 0, NULL);
    run(NULL);
    WaitForMultipleObjects(n, t, TRUE, INFINITE);
    for (i = 0; i < n; i++) CloseHandle(t[i]);
    GetProcessHandleCount(GetCurrentProcess(), &after);
    if (after > before) { printf("handle count grew: %lu -> %lu\n", before, after); return 1; }
    if (bad) { printf("%ld calls with the wrong id\n", bad); return 1; }
    printf("msvcp120 at %p\n", m);
    printf("thrd ok %d %d zero=%ld\n", n + 1, calls, zero);
    if (argc > 3) {
        int tn = atoi(argv[3]), r;
        LARGE_INTEGER f, a, b;
        volatile unsigned long long sink = 0;
        QueryPerformanceFrequency(&f);
        for (r = 0; r < 5; r++) {
            QueryPerformanceCounter(&a);
            for (i = 0; i < tn; i++) sink += thrd_current();
            QueryPerformanceCounter(&b);
            printf("thrd ns %.1f (round %d, %d calls)\n", (double)(b.QuadPart - a.QuadPart) * 1e9 / f.QuadPart / tn, r, tn);
        }
    }
    return 0;
}
