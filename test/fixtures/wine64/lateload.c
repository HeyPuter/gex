/* A native DLL loaded late, its functions called from guest code on a
 * thread that has made no system call since (its table has not caught up
 * with the DLL's install), then unloaded, loaded again and called again,
 * from both threads. The guest's calls go to the DLL's table indices
 * (conv_table()'s pointers), the native module's entry path. */
#include <windows.h>
#include <stdio.h>
#include "conv.h"

static int failures;
static void check(int ok, const char *what, long long got)
{
    printf("%s %s (%lld)\n", ok ? "ok" : "FAIL", what, got);
    if (!ok) failures++;
}

static int (*volatile g_fn)(int, int, int, int, int, int);
static long long (*volatile g_fn12)(int, long long, int, long long, int, long long, int, long long, int, long long, int, long long);
static volatile int round_, spinning, result;
static volatile long long result12;

/* spins on g_fn for each round, never entering the kernel, and calls it */
static DWORD WINAPI spin(void *arg)
{
    for (int r = 1; r <= 2; r++)
    {
        int (*p)(int, int, int, int, int, int);
        while (round_ != r) ;
        spinning = r;
        while (!(p = g_fn)) ;
        result = p(1, 2, 3, 4, 5, 6);
        result12 = g_fn12(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
        g_fn = NULL;
        spinning = -r;
    }
    return 0;
}

static int load_and_call(int r, HMODULE *out)
{
    const struct conv_table *(*get)(void);
    const struct conv_table *t;
    HMODULE m;

    round_ = r;
    while (spinning != r) Sleep(1);
    Sleep(50);
    if (!(m = LoadLibraryA("convdll.dll")) || !(get = (void *)GetProcAddress(m, "conv_table")))
    {
        printf("FAIL LoadLibrary convdll.dll round %d: %lu\n", r, GetLastError());
        failures++;
        return 0;
    }
    t = get();
    g_fn12 = t->c_i12;
    g_fn = t->c_i6;
    for (int i = 0; i < 2000 && spinning != -r; i++) Sleep(10);
    check(spinning == -r, r == 1 ? "the spinning thread's call returned" : "the spinning thread's call returned, reloaded", spinning);
    check(result == 91, r == 1 ? "6 int arguments, from a thread that had not caught up" : "6 int arguments after a reload", result);
    check(result12 == 650, r == 1 ? "12 arguments, from that thread" : "12 arguments after a reload", result12);
    check(t->c_i6(6, 5, 4, 3, 2, 1) == 56, "6 int arguments from the loading thread", t->c_i6(6, 5, 4, 3, 2, 1));
    *out = m;
    return 1;
}

int main(void)
{
    HANDLE th = CreateThread(NULL, 0, spin, NULL, 0, NULL);
    HMODULE m;

    if (load_and_call(1, &m))
    {
        check(FreeLibrary(m), "FreeLibrary", 1);
        check(GetModuleHandleA("convdll.dll") == NULL, "unloaded", 0);
        load_and_call(2, &m);
    }
    WaitForSingleObject(th, 20000);
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
