/* A thread that runs on across another thread's unit install
 * (tests/w3/ftabstale.sh, X's B16). Unit code calls a function pointer
 * through the process-wide probe cache straight into its own Worker's
 * function table, so a thread that last caught up before the install calls
 * an index past its table's end. The worker thread makes its last system
 * call (SetEvent), then spins on a flag in this exe's own unit code. The
 * main thread loads ftabdll.dll (whose unit the test built), gets its unit
 * installed, and calls ftab_sum through a pointer, which publishes the
 * entry's index. Then it sets the flag, and the worker calls through the
 * same pointer, with no system call between. Prints "ftabdll at BASE", then
 * "ftabstale ok S". */
#include <windows.h>
#include <stdio.h>

typedef unsigned (*sum_fn)(unsigned);
static sum_fn volatile fp;
static volatile LONG flag;
static volatile unsigned got;
static HANDLE ready;

static DWORD WINAPI worker(void *arg)
{
    SetEvent(ready);
    while (!flag) ;
    got = fp(7);
    return 0;
}

int main(void)
{
    HANDLE t;
    HMODULE m;
    unsigned want, i;

    ready = CreateEventA(NULL, TRUE, FALSE, NULL);
    t = CreateThread(NULL, 0, worker, NULL, 0, NULL);
    WaitForSingleObject(ready, INFINITE);
    if (!(m = LoadLibraryA("ftabdll.dll"))) { printf("no ftabdll.dll (%lu)\n", GetLastError()); return 1; }
    printf("ftabdll at %p\n", m);
    fflush(stdout);
    if (!(fp = (sum_fn)GetProcAddress(m, "ftab_sum"))) { printf("no ftab_sum\n"); return 1; }
    /* The unit is served at this thread's next turn out (the sleeps), and
     * the calls after publish the entry's index. */
    for (i = 0; i < 4; i++) { fp(i); Sleep(50); }
    want = fp(7);
    InterlockedExchange(&flag, 1);
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    if (got != want) { printf("ftab_sum(7) %u on the worker, %u on main\n", got, want); return 1; }
    printf("ftabstale ok %u\n", got);
    return 0;
}
