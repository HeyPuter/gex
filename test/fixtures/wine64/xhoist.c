/* A thread in a long loop of calls through a pointer, with the ICALL bound's
 * load (the thread's own table size) hoisted out of the loop (GexIcall marks
 * it invariant), while another thread starts a thread and loads a DLL, and
 * then points the loop at the DLL's function. The looping thread makes no
 * system call: its hoisted bound is stale, its table has not caught up, and
 * the call must reach the function through the exit stub's own-table check
 * and the call-out's catch-up, never an index its table lacks. Prints ok
 * lines and "0 failures", or traps. */
#include <windows.h>
#include <stdio.h>
static int local2(int x) { return x + 2; }
static int (*volatile g_slot)(int) = local2;
static volatile LONG g_stop, g_started;
static volatile LONGLONG g_local, g_dll;
/* (not inlined: the loop is its own function, the bound's load before it) */
__attribute__((noinline)) static void run(int (*volatile *slot)(int), volatile LONG *stop)
{
    LONGLONG nl = 0, nd = 0;
    int i = 0;
    while (!*stop)
    {
        int r = (*slot)(i);
        if (r == i + 2) g_local = ++nl;
        else if (r == i + 1) g_dll = ++nd;
        else { g_dll = -1000000000; return; }
        i = (i + 1) & 0xffff;
    }
}
static DWORD WINAPI loop(void *arg) { g_started = 1; run(&g_slot, &g_stop); return 0; }
static DWORD WINAPI idle(void *arg) { Sleep(50); return 0; }
int main(int argc, char **argv)
{
    HANDLE t = CreateThread(NULL, 0, loop, NULL, 0, NULL), u;
    HMODULE m;
    int (*(*get)(void))(int);
    DWORD start;
    while (!g_started || g_local < 100000) Sleep(1);
    /* (a thread started meanwhile) */
    u = CreateThread(NULL, 0, idle, NULL, 0, NULL);
    if (!(m = LoadLibraryA("xthr_dll.dll"))) { printf("FAIL LoadLibrary xthr_dll.dll (%lu)\n1 failures\n", GetLastError()); return 1; }
    WaitForSingleObject(u, 10000);
    get = (int (*(*)(void))(int))GetProcAddress(m, "xthr_get");
    g_slot = get();
    start = GetTickCount();
    while (g_dll >= 0 && g_dll < 100000 && GetTickCount() - start < 60000) Sleep(1);
    g_stop = 1;
    WaitForSingleObject(t, 10000);
    if (g_dll < 100000) { printf("FAIL calls into the DLL from the loop (%lld)\n1 failures\n", (long long)g_dll); return 1; }
    printf("ok calls into the DLL from the loop (%lld)\n0 failures\n", (long long)g_dll);
    return 0;
}
