/* Many processes making and closing handles, with door calls whose handlers
 * read the clock in the door (new_thread, select): the server's heap must
 * stay whole. A door context shorter than gex's register context let the
 * kernel's LAST_SYSCALL write land in the next heap block, a process's
 * handle table, and close_handle asserted (docs/wineserver-kernel.md
 * §10.2). Prints "handles ok N".
 *
 *   handles.exe [N]     the parent: N children at once (default 6)
 *   handles.exe -c      a child */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static DWORD WINAPI worker(void *arg)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    SetEvent((HANDLE)arg);
    return 0;
}

static int child(void)
{
    int i;
    for (i = 0; i < 150; i++)
    {
        HANDLE ev = CreateEventA(NULL, TRUE, FALSE, NULL), th, dup;
        if (!ev) return 1;
        if (!(th = CreateThread(NULL, 0, worker, ev, 0, NULL))) return 2;
        if (WaitForSingleObject(ev, 10000) != WAIT_OBJECT_0) return 3;
        if (WaitForSingleObject(th, 10000) != WAIT_OBJECT_0) return 4;
        if (!DuplicateHandle(GetCurrentProcess(), ev, GetCurrentProcess(), &dup, 0, FALSE, DUPLICATE_SAME_ACCESS)) return 5;
        if (!CloseHandle(dup) || !CloseHandle(th) || !CloseHandle(ev)) return 6;
    }
    return 0;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 16], self[MAX_PATH];
    HANDLE procs[16];
    int n = argc > 1 ? atoi(argv[1]) : 6, i, ok = 0;

    if (argc > 1 && !strcmp(argv[1], "-c")) return child();
    if (n < 1 || n > 16) n = 6;
    GetModuleFileNameA(NULL, self, sizeof(self));
    sprintf(cmd, "\"%s\" -c", self);
    for (i = 0; i < n; i++)
    {
        STARTUPINFOA si = { sizeof(si) };
        PROCESS_INFORMATION pi;
        if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) { printf("handles: no child %d\n", i); return 1; }
        CloseHandle(pi.hThread);
        procs[i] = pi.hProcess;
    }
    for (i = 0; i < n; i++)
    {
        DWORD rc = 1;
        WaitForSingleObject(procs[i], INFINITE);
        GetExitCodeProcess(procs[i], &rc);
        if (rc) printf("handles: child %d failed (%lu)\n", i, rc); else ok++;
        CloseHandle(procs[i]);
    }
    if (ok != n) return 1;
    printf("handles ok %d\n", n);
    return 0;
}
