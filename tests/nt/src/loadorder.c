/* WINEDLLOVERRIDES: with no override the winhttp.dll beside the program
 * loads (it is found first); with winhttp=b the prefix's builtin does; with
 * winhttp=d none does. argv[1] says which to expect: native, builtin, none. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

int main(int argc, char **argv) {
    const char *want = argc > 1 ? argv[1] : "native";
    HMODULE h = LoadLibraryA("winhttp.dll");
    int local = h && GetProcAddress(h, "gex_loadorder_marker");
    int builtin = h && GetProcAddress(h, "WinHttpOpen");
    if (!strcmp(want, "native")) CHECK("the program's own winhttp.dll", local && !builtin);
    else if (!strcmp(want, "builtin")) CHECK("the builtin winhttp.dll", builtin && !local);
    else CHECK("no winhttp.dll", !h);
    printf("%d failures\n", failures);
    return failures;
}
