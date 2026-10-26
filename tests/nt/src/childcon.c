/* A console launcher: a GUI test (user_win.exe, beside this one) as a child
 * process, waited for, its exit code passed on. Unlike childgui, it makes no
 * user32 call itself, so the child is the first process with a desktop and
 * hosts the desktop thread (docs/wine-native.md: child sessions). Under
 * native Wine on gex (32-bit Wine) the child reads the session through wineserver
 * requests. Its stdout is inherited.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
int main(int argc, char **argv) {
    char cmd[MAX_PATH + 16];
    STARTUPINFOA si = {sizeof si};
    PROCESS_INFORMATION pi;
    DWORD code = 99;
    snprintf(cmd, sizeof cmd, "%s", argc > 1 ? argv[1] : "user_win.exe");
    fflush(stdout);
    if (!CreateProcessA(0, cmd, 0, 0, TRUE, 0, 0, 0, &si, &pi)) {
        printf("FAIL CreateProcess %lu\n", GetLastError());
        return 98;
    }
    WaitForSingleObject(pi.hProcess, INFINITE);
    GetExitCodeProcess(pi.hProcess, &code);
    printf("child exit %lu\n", code);
    return code;
}
