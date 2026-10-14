/* The wineserver's clock as clients see it (srvtime.sh): times the server
 * stamps (a thread's creation time), wait timeouts and waitable timers, with
 * plain requests in between. Under doors the server reads its clock lazily,
 * only when a request looks at it (DOOR_VERSION 2, docs/wine-overhead.md).
 * Prints "srvtime ok R" when every round agrees with the client's clocks.
 * usage: srvtime.exe [ROUNDS] */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <stdlib.h>

static LONGLONG ms_now(void)
{
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c); QueryPerformanceFrequency(&f);
    return c.QuadPart * 1000 / f.QuadPart;
}

static ULONGLONG ft_now(void)
{
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    return ((ULONGLONG)ft.dwHighDateTime << 32) | ft.dwLowDateTime;
}

static DWORD WINAPI idle(void *arg) { (void)arg; return 0; }

typedef NTSTATUS (WINAPI *qit_fn)(HANDLE, THREADINFOCLASS, void *, ULONG, ULONG *);

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 5, r, i;
    qit_fn qit = (qit_fn)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryInformationThread");
    HANDLE ev = CreateEventA(NULL, TRUE, FALSE, NULL), tm = CreateWaitableTimerA(NULL, TRUE, NULL);
    char tbi[64];
    for (r = 0; r < rounds; r++)
    {
        ULONGLONG before, after, created;
        FILETIME c, e, k, u;
        LARGE_INTEGER due;
        LONGLONG t0, dt;
        HANDLE th;
        DWORD w;

        /* requests that never look at the time: the server's clock stays unread */
        for (i = 0; i < 1000; i++) qit(GetCurrentThread(), 0, tbi, 48, NULL);
        Sleep(20);

        /* a time the server stamps, inside the client's window (10 ms slack) */
        before = ft_now();
        th = CreateThread(NULL, 0, idle, NULL, 0, NULL);
        after = ft_now();
        if (!th || !GetThreadTimes(th, &c, &e, &k, &u)) { printf("srvtime: thread times failed\n"); return 1; }
        created = ((ULONGLONG)c.dwHighDateTime << 32) | c.dwLowDateTime;
        if (created + 100000 < before || created > after + 100000)
        {
            printf("srvtime: round %d creation time %llu outside [%llu, %llu]\n", r, created, before, after);
            return 1;
        }
        WaitForSingleObject(th, INFINITE); CloseHandle(th);

        for (i = 0; i < 1000; i++) qit(GetCurrentThread(), 0, tbi, 48, NULL);
        Sleep(20);

        /* a wait's timeout: not early, not lost */
        t0 = ms_now();
        w = WaitForSingleObject(ev, 60);
        dt = ms_now() - t0;
        if (w != WAIT_TIMEOUT || dt < 55 || dt > 5000) { printf("srvtime: round %d wait 60 ms: %lu after %lld ms\n", r, w, dt); return 1; }

        /* a relative timer */
        due.QuadPart = -500000;
        t0 = ms_now();
        if (!SetWaitableTimer(tm, &due, 0, NULL, NULL, FALSE)) { printf("srvtime: timer failed\n"); return 1; }
        w = WaitForSingleObject(tm, 5000);
        dt = ms_now() - t0;
        if (w != WAIT_OBJECT_0 || dt < 45 || dt > 5000) { printf("srvtime: round %d relative timer 50 ms: %lu after %lld ms\n", r, w, dt); return 1; }

        for (i = 0; i < 1000; i++) qit(GetCurrentThread(), 0, tbi, 48, NULL);
        Sleep(20);

        /* an absolute timer: the server's wall clock against the client's */
        due.QuadPart = ft_now() + 500000;
        t0 = ms_now();
        if (!SetWaitableTimer(tm, &due, 0, NULL, NULL, FALSE)) { printf("srvtime: timer failed\n"); return 1; }
        w = WaitForSingleObject(tm, 5000);
        dt = ms_now() - t0;
        if (w != WAIT_OBJECT_0 || dt < 40 || dt > 5000) { printf("srvtime: round %d absolute timer 50 ms: %lu after %lld ms\n", r, w, dt); return 1; }
    }
    printf("srvtime ok %d\n", rounds);
    return 0;
}
