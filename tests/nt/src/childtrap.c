/* A child process that dies in native code (a function pointer the native
 * C runtime cannot call: a trap under gex) must release whoever waits for
 * it. An exec'd process that trapped used to keep its descriptors open,
 * its server connection among them, so native Wine's server never saw it
 * go and the parent waited for ever. Native Wine (W3) only:
 * GEX_NT_CHILDREN=1 runs it.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    char cmd[MAX_PATH + 16], self[MAX_PATH];
    STARTUPINFOA si = {sizeof si};
    PROCESS_INFORMATION pi;
    DWORD code = STILL_ACTIVE, w;
    if (argc > 1) {
        int v[2] = {2, 1};
        qsort(v, 2, sizeof(int), (int (*)(const void *, const void *))(ULONG_PTR)5);
        return 0;
    }
    GetModuleFileNameA(0, self, sizeof self);
    snprintf(cmd, sizeof cmd, "\"%s\" child", self);
    if (!CreateProcessA(0, cmd, 0, 0, TRUE, 0, 0, 0, &si, &pi)) return 98;
    w = WaitForSingleObject(pi.hProcess, 120000);
    GetExitCodeProcess(pi.hProcess, &code);
    printf(w == WAIT_OBJECT_0 && code != STILL_ACTIVE && code != 0 ? "child gone\n" : "FAIL wait %lu exit %lx\n", w, code);
    return 0;
}
