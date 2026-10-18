/* tests/w3/dllband: the DLLs a W3 bundle pins load in the band.
 *
 * dllband.sh writes the prefix's dll-bases with tools/wine-wasm/w3/
 * dll-bases.py for the DLLs below, and this loads them: each must be at the
 * address the file gives it, in [0x11000000, 0x20000000), where gex's compact
 * space keeps an island for them (docs/wine-native.md §3.19), not at its own
 * base (w3-bases.txt, 0x20000000 and up), where the browser shim's heap may
 * already be. Prints "dllband ok". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;

#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

static const char *const dlls[] = {"version.dll", "winmm.dll", "dbghelp.dll", "opengl32.dll"};

int main(int argc, char **argv) {
    char what[256];
    /* argv[1]: the dll-bases file as written, to compare against */
    FILE *f = argc > 1 ? fopen(argv[1], "r") : NULL;
    char line[1024];
    CHECK("the dll-bases file is readable", f != NULL);
    for (unsigned i = 0; i < sizeof(dlls) / sizeof(dlls[0]); i++) {
        HMODULE h = LoadLibraryA(dlls[i]);
        ULONG_PTR at = (ULONG_PTR)h;
        ULONG_PTR want = 0;
        if (f) {
            rewind(f);
            while (fgets(line, sizeof(line), f)) {
                char *sp = strchr(line, ' ');
                char *name = sp ? strrchr(sp, '/') : NULL;
                if (name && !_strnicmp(name + 1, dlls[i], strlen(dlls[i])) && name[1 + strlen(dlls[i])] < ' ') {
                    want = strtoul(line, NULL, 16);
                    break;
                }
            }
        }
        snprintf(what, sizeof(what), "%s at %#lx, its pin %#lx, in [0x11000000, 0x20000000)", dlls[i],
                 (unsigned long)at, (unsigned long)want);
        CHECK(what, h && want && at == want && at >= 0x11000000 && at < 0x20000000);
    }
    if (f) fclose(f);
    if (!failures) printf("dllband ok\n");
    else printf("dllband: %d failed\n", failures);
    return failures != 0;
}
