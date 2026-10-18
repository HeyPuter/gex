/* tests/wine64/dllband: the DLLs a W64 prefix pins load at their pins, in
 * every process.
 *
 * dllband.sh writes the prefix's dll-bases with tools/wine-wasm/w64/
 * dll-bases.py for the DLLs below, and this loads them, then starts itself
 * again ("child") and the child loads them too: each must be at the address
 * the file gives it, in [0x11000000, 0x20000000), in both processes. Without
 * the pins an x86_64 builtin, whose own base (0x180000000) is beyond the
 * 4 GiB W64 is lowered to, lands top-down in the order a process loads
 * images, so its address differs between processes and a function-tier unit
 * (keyed by the image and its base) misses. Prints "dllband ok". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;

#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

/* (opengl32 has a base below 4 GiB of its own, 0x7a800000; the pin wins) */
static const char *const dlls[] = {"version.dll", "winmm.dll", "dbghelp.dll", "opengl32.dll"};

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
    FILE *f = file ? fopen(file, "r") : NULL;
    snprintf(what, sizeof(what), "%s: the dll-bases file is readable", who);
    CHECK(what, f != NULL);
    for (unsigned i = 0; i < sizeof(dlls) / sizeof(dlls[0]); i++) {
        HMODULE h = LoadLibraryA(dlls[i]);
        ULONG_PTR at = (ULONG_PTR)h, want = pin_of(f, dlls[i]);
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
            WaitForSingleObject(pi.hProcess, 60000);
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
