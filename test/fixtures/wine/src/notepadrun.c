/* Wine's notepad as a child, shown for a while and closed: a GUI program a
 * test can wait for. Starts `notepad.exe` (or argv[2]), waits up to 600 s
 * for its top-level window (class "Notepad"), prints its title, leaves it
 * up for argv[1] seconds (default 5: the display's frames are checked
 * against it, menus and text drawn), then sends WM_CLOSE and waits for it
 * to exit: "closed N" with its exit code. Exit 0 when all of that went.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 5, i;
    char cmd[MAX_PATH + 16];
    wchar_t title[128];
    STARTUPINFOA si = {sizeof si};
    PROCESS_INFORMATION pi;
    DWORD code = 99;
    HWND w = 0;
    snprintf(cmd, sizeof cmd, "%s", argc > 2 ? argv[2] : "notepad.exe");
    GetDesktopWindow();
    if (!CreateProcessA(0, cmd, 0, 0, FALSE, 0, 0, 0, &si, &pi)) {
        printf("FAIL CreateProcess %lu\n", GetLastError());
        return 98;
    }
    for (i = 0; i < 6000 && !(w = FindWindowW(L"Notepad", 0)); i++) Sleep(100);
    if (!w) { printf("FAIL no Notepad window\n"); TerminateProcess(pi.hProcess, 1); return 97; }
    GetWindowTextW(w, title, ARRAYSIZE(title));
    printf("title %ls\n", title);
    fflush(stdout);
    Sleep(secs * 1000);
    PostMessageW(w, WM_CLOSE, 0, 0);
    if (WaitForSingleObject(pi.hProcess, 60000) != WAIT_OBJECT_0) {
        printf("FAIL still running after WM_CLOSE\n");
        TerminateProcess(pi.hProcess, 1);
        return 96;
    }
    GetExitCodeProcess(pi.hProcess, &code);
    printf("closed %lu\n", code);
    return code ? 95 : 0;
}
