/* The console launcher wine32/prefix.sh runs Wine's install under: its
 * command line (argv[1]) as a child process, waited for, then "child exit N"
 * printed and N returned. It makes no user32 call itself, so the child is the
 * first process with a desktop and hosts the desktop thread. The child's
 * stdout is inherited.
 * prefix.sh builds it: clang --target=i686-w64-mingw32 -fuse-ld=lld -O2 -s
 * -Wl,--no-insert-timestamp (no PE timestamp, so the same source gives the
 * same file). */
#include <windows.h>
#include <stdio.h>
int main(int argc, char **argv) {
    char cmd[MAX_PATH + 16];
    STARTUPINFOA si = {sizeof si};
    PROCESS_INFORMATION pi;
    DWORD code = 99;
    if (argc < 2) {
        printf("usage: launch.exe COMMAND\n");
        return 97;
    }
    snprintf(cmd, sizeof cmd, "%s", argv[1]);
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
