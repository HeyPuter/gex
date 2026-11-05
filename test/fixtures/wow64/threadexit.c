/* A thread that ends inside its start routine's call-out, after native code
 * called back into it: the process must go on.
 *
 * Under 32-bit Wine (Wine as native wasm32) a thread's x86 start routine runs in a
 * call-out from native ntdll; qsort (native msvcrt) calls the x86 comparator
 * in nested call-outs. When the routine returns, RtlExitUserThread's
 * pthread_exit ends the thread with those call-outs' frames below it. A
 * resume left from a nested call-out's return used to be entered after the
 * exit: the guest ran into the call-out return pad's breakpoint and the
 * process died with status 133 (winedevice and services in every page run).
 *
 * Prints "threads ok N" and exits 0. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static volatile LONG sum;

static int __cdecl cmp(const void *a, const void *b)
{
    return *(const int *)a - *(const int *)b;
}

static DWORD WINAPI worker(void *arg)
{
    int v[16], i;
    for (i = 0; i < 16; i++) v[i] = (i * 7919 + (int)(INT_PTR)arg) % 101;
    qsort(v, 16, sizeof(int), cmp);
    for (i = 1; i < 16; i++) if (v[i - 1] > v[i]) return 1;
    InterlockedIncrement(&sum);
    return 0;   /* ends the thread under its call-out */
}

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 8, i;
    for (i = 0; i < n; i++)
    {
        DWORD code = 99;
        HANDLE h = CreateThread(NULL, 0, worker, (void *)(INT_PTR)i, 0, NULL);
        if (!h) { printf("CreateThread failed %lu\n", GetLastError()); return 2; }
        WaitForSingleObject(h, INFINITE);
        GetExitCodeThread(h, &code);
        CloseHandle(h);
        if (code != 0) { printf("thread %d exit code %lu\n", i, code); return 3; }
    }
    Sleep(200);   /* the process must still be here after its threads ended */
    printf("threads ok %ld\n", sum);
    fflush(stdout);
    return 0;
}
