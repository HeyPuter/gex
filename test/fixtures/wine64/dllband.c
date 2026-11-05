/* The DLLs a 64-bit Wine prefix pins load at their pins, in every process
 * (test/wine64/dllband.test.js).
 *
 * The test writes the prefix's dll-bases with wine/dll-bases.py for the
 * DLLs below, and this loads them, then starts itself
 * again ("child") and the child loads them too: each must be at the address
 * the file gives it, in [0x11000000, 0x20000000), in both processes. Without
 * the pins an x86_64 builtin, whose own base (0x180000000) is beyond the
 * 4 GiB 64-bit Wine is lowered to, lands top-down in the order a process loads
 * images, so its address differs between processes and a function-tier unit
 * (keyed by the image and its base) misses. One of them is a program's own
 * DLL in a directory whose name has a space (C:\pin dir\pinme.dll): its pin
 * names a unix path with a space in it. Another (fixed.dll) has no dynamic
 * base and its own base (FIXED_BASE) is taken, by this program's reservation
 * made before it loads: it is relocated to its pin, not to wherever there is
 * room. Prints "dllband ok". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;
/* When the process started, and each check's time since, with the time the
 * check itself took: where a slow child's minute goes. */
static ULONGLONG t0, last;

#define CHECK(name, cond) do { int ok_ = (cond); ULONGLONG now_ = GetTickCount64(); \
    printf("%s %s [%.1f s, +%.1f s]\n", ok_ ? "ok  " : "FAIL", name, (now_ - t0) / 1000.0, (now_ - last) / 1000.0); \
    fflush(stdout); last = now_; if (!ok_) failures++; } while (0)

/* (opengl32 has a base below 4 GiB of its own, 0x7a800000; the pin wins) */
static const char *const dlls[] = {"version.dll", "winmm.dll", "dbghelp.dll", "opengl32.dll", "C:\\pin dir\\pinme.dll",
                                   "C:\\pin dir\\fixed.dll"};

/* fixed.dll's own base (the test links it there, without a dynamic base) */
#define FIXED_BASE 0x30000000

/* The file name of a DLL as loaded: what its pin's path ends in. */
static const char *leaf(const char *dll) {
    const char *b = strrchr(dll, '\\');
    return b ? b + 1 : dll;
}

static ULONG_PTR pin_of(FILE *f, const char *dll) {
    char line[1024];
    if (!f) return 0;
    rewind(f);
    while (fgets(line, sizeof(line), f)) {
        char *sp = strchr(line, ' ');
        char *name = sp ? strrchr(sp, '/') : NULL;
        if (name && !_strnicmp(name + 1, dll, strlen(dll)) && (unsigned char)name[1 + strlen(dll)] <= ' ')
            return (ULONG_PTR)_strtoui64(line, NULL, 16);
    }
    return 0;
}

int main(int argc, char **argv) {
    char what[512];
    int child = argc > 2 && !strcmp(argv[1], "child");
    const char *file = child ? argv[2] : argc > 1 ? argv[1] : NULL;
    const char *who = child ? "child" : "parent";
    FILE *f;
    t0 = last = GetTickCount64();
    f = file ? fopen(file, "r") : NULL;
    {
        /* how long it took to get here: the loader's share */
        FILETIME made, x, y, z, now;
        GetProcessTimes(GetCurrentProcess(), &made, &x, &y, &z);
        GetSystemTimeAsFileTime(&now);
        snprintf(what, sizeof(what), "%s: at main %.1f s after the process was made", who,
                 (double)(((ULONGLONG)now.dwHighDateTime << 32 | now.dwLowDateTime) -
                          ((ULONGLONG)made.dwHighDateTime << 32 | made.dwLowDateTime)) / 1e7);
        CHECK(what, 1);
    }
    snprintf(what, sizeof(what), "%s: the dll-bases file is readable", who);
    CHECK(what, f != NULL);
    /* fixed.dll's own base taken before it loads */
    snprintf(what, sizeof(what), "%s: %#x reserved", who, FIXED_BASE);
    CHECK(what, VirtualAlloc((void *)FIXED_BASE, 0x100000, MEM_RESERVE, PAGE_NOACCESS) == (void *)FIXED_BASE);
    /* DLLBAND_HANG=1: the child never gets further (to try the test's hang path) */
    if (child && getenv("DLLBAND_HANG")) Sleep(INFINITE);
    for (unsigned i = 0; i < sizeof(dlls) / sizeof(dlls[0]); i++) {
        HMODULE h = LoadLibraryA(dlls[i]);
        ULONG_PTR at = (ULONG_PTR)h, want = pin_of(f, leaf(dlls[i]));
        snprintf(what, sizeof(what), "%s: %s at %#llx, its pin %#llx, in [0x11000000, 0x20000000)", who, dlls[i],
                 (unsigned long long)at, (unsigned long long)want);
        CHECK(what, h && want && at == want && at >= 0x11000000 && at < 0x20000000);
    }
    if (f) fclose(f);
    if (child) return failures != 0;
    /* a second process maps the same images: the same addresses */
    {
        char cmd[MAX_PATH * 2 + 16], self[MAX_PATH];
        STARTUPINFOA si = {sizeof(si)};
        PROCESS_INFORMATION pi;
        DWORD code = 99;
        GetModuleFileNameA(NULL, self, sizeof(self));
        snprintf(cmd, sizeof(cmd), "\"%s\" child %s", self, file ? file : "");
        if (CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
            /* A child still running after three minutes is said so (the
             * test's FAIL names the hang), and given up two minutes later. */
            if (WaitForSingleObject(pi.hProcess, 180000) == WAIT_TIMEOUT) {
                printf("dllband: the child still runs after 180 s\n");
                fflush(stdout);
                Sleep(120000);
            }
            GetExitCodeProcess(pi.hProcess, &code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        snprintf(what, sizeof(what), "parent: the child found them at their pins too (exit %lu)", (unsigned long)code);
        CHECK(what, code == 0);
    }
    if (!failures) printf("dllband ok\n");
    else printf("dllband: %d failed\n", failures);
    return failures != 0;
}
