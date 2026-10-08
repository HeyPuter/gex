/* A launcher: a GUI test (user_win.exe, beside this one) as a child process,
 * waited for, its exit code passed on. Under native Wine on gex (W3) the
 * child cannot map the session's shared memory and reads it through
 * wineserver requests (docs/wine-native.md). Its stdout is inherited.
 * As a GUI launcher is, this one is a user32 program that has the desktop
 * before it starts the child (GetDesktopWindow): the desktop then lives in
 * the process hosting the server.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
int main(int argc, char **argv) {
    char cmd[MAX_PATH + 16];
    STARTUPINFOA si = {sizeof si};
    PROCESS_INFORMATION pi;
    DWORD code = 99;
    snprintf(cmd, sizeof cmd, "%s", argc > 1 ? argv[1] : "user_win.exe");
    GetDesktopWindow();
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
