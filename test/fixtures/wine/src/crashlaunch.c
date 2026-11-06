/* Starts argv[1] with argv[2] (test/wow64/crash, test/wine/crash), waits for its window
 * (class "crash") and then for it to end, and says how it ended: its exit
 * code, how long after its window showed, and whether the window is gone. */
#include <windows.h>
#include <stdio.h>
#include <stdarg.h>

/* One line in one write: the child's output and gex's go to the same place. */
static void say(const char *fmt, ...)
{
    char line[256];
    DWORD n;
    va_list a;

    va_start(a, fmt);
    n = vsnprintf(line, sizeof(line), fmt, a);
    va_end(a);
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), line, n < sizeof(line) ? n : sizeof(line) - 1, &n, NULL);
}

int main(int argc, char **argv)
{
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    char cmd[512];
    DWORD t0, r, code = 0;

    if (argc < 2) return 2;
    snprintf(cmd, sizeof(cmd), "%s %s", argv[1], argc > 2 ? argv[2] : "");
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        say("launch: CreateProcess failed %lu\n", GetLastError());
        return 1;
    }
    while (!FindWindowA("crash", NULL) && WaitForSingleObject(pi.hProcess, 50) == WAIT_TIMEOUT) ;
    say("launch: window %s\n", FindWindowA("crash", NULL) ? "up" : "never up");
    t0 = GetTickCount();
    r = WaitForSingleObject(pi.hProcess, 90000);
    GetExitCodeProcess(pi.hProcess, &code);
    say("launch: %s, exit 0x%lx, %lu ms\n", r == WAIT_OBJECT_0 ? "ended" : "still running", code, GetTickCount() - t0);
    say("launch: window %s\n", FindWindowA("crash", NULL) ? "still there" : "gone");
    return 0;
}
