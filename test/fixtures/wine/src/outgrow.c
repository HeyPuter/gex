/* outgrow: the Windows guest of test/wow64/outgrow. Reserves 3.5 GiB in
 * one block (VirtualAlloc, no address), which the lowered memory has no
 * room for and a 64-bit one has.
 *
 *   outgrow crash      no room: says "outgrow short", then raises an
 *                      access violation no handler takes (gex maps no
 *                      guard at address 0 unless asked to)
 *   outgrow fallback   no room: reserves 64 MiB blocks instead until one
 *                      is refused, says "outgrow fallback ok", then
 *                      reserves 64 KiB at 0x7ff00000, just below Windows'
 *                      old KUSER_SHARED_DATA page, and says "outgrow
 *                      reached 2g" when it got it; exits 0
 *   outgrow twice M    runs `outgrow M` twice, one after the other
 *                      (CreateProcess each), and says how each ended
 *   outgrow each M X.. the same for each program X (a copy of this one),
 *                      each start the moment the one before ends
 *
 * Each run says "outgrow big high" when it got the 3.5 GiB above 4 GiB.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define BIG ((SIZE_T)7 << 29)

static void once(const char *exe, const char *mode) {
    char me[MAX_PATH], cmd[MAX_PATH + 32];
    STARTUPINFOA si = { sizeof si };
    PROCESS_INFORMATION pi;
    DWORD code = 0;

    fflush(stdout);
    if (exe) snprintf(me, sizeof me, "%s", exe);
    else GetModuleFileNameA(NULL, me, sizeof me);
    snprintf(cmd, sizeof cmd, "\"%s\" %s", me, mode);
    if (!CreateProcessA(me, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        printf("outgrow child not started: %lu\n", GetLastError());
        return;
    }
    WaitForSingleObject(pi.hProcess, INFINITE);
    GetExitCodeProcess(pi.hProcess, &code);
    printf("outgrow child exit %08lx\n", code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "crash";
    void *p;

    if (argc > 2 && !strcmp(argv[1], "twice")) {
        once(NULL, argv[2]);
        once(NULL, argv[2]);
        return 0;
    }
    if (argc > 3 && !strcmp(argv[1], "each")) {
        int i;
        for (i = 3; i < argc; i++) {
            once(argv[i], argv[2]);
            once(argv[i], argv[2]);
        }
        return 0;
    }
    p = VirtualAlloc(NULL, BIG, MEM_RESERVE, PAGE_NOACCESS);
    if (p) {
        printf("outgrow big %s\n", (ULONG_PTR)p >= ((ULONG_PTR)1 << 32) ? "high" : "low");
        return 0;
    }
    if (!strcmp(mode, "crash")) {
        printf("outgrow short\n");
        fflush(stdout);
        RaiseException(EXCEPTION_ACCESS_VIOLATION, EXCEPTION_NONCONTINUABLE, 0, NULL);
    }
    {
        ULONG_PTR top = 0;
        int n = 0;

        while ((p = VirtualAlloc(NULL, 64 << 20, MEM_RESERVE, PAGE_NOACCESS))) {
            if ((ULONG_PTR)p + (64 << 20) > top) top = (ULONG_PTR)p + (64 << 20);
            n++;
        }
        printf("outgrow fallback %s\n", n ? "ok" : "failed");
        /* a block just below the old KUSER page, at its address: the
         * memory then reaches past that page */
        if (VirtualAlloc((void *)0x7ff00000, 0x10000, MEM_RESERVE, PAGE_NOACCESS)) top = 0x7ff10000;
        if (top > 0x7f000000) printf("outgrow reached 2g\n");
    }
    return 0;
}
