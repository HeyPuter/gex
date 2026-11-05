/* breakin: a debugger's two ways of breaking into a running x64 child,
 * DebugActiveProcess (its attach's break-in) and DebugBreakProcess, each a
 * remote thread at ntdll's DbgUiRemoteBreakin that must reach the debugger
 * as a breakpoint. One "ok"/"FAIL" line a check, "N failures" last.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>

static int failures;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

/* Debug events until a breakpoint (1), the child's exit (0) or a timeout (-1). */
static int until_breakpoint(void)
{
    DEBUG_EVENT ev;
    while (WaitForDebugEvent(&ev, 30000)) {
        DWORD cont = DBG_CONTINUE;
        int r = -2;
        if (ev.dwDebugEventCode == EXCEPTION_DEBUG_EVENT) {
            if (ev.u.Exception.ExceptionRecord.ExceptionCode == EXCEPTION_BREAKPOINT) r = 1;
            else cont = DBG_EXCEPTION_NOT_HANDLED;
        }
        if (ev.dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT) r = 0;
        if (ev.dwDebugEventCode == CREATE_PROCESS_DEBUG_EVENT && ev.u.CreateProcessInfo.hFile)
            CloseHandle(ev.u.CreateProcessInfo.hFile);
        if (ev.dwDebugEventCode == LOAD_DLL_DEBUG_EVENT && ev.u.LoadDll.hFile) CloseHandle(ev.u.LoadDll.hFile);
        ContinueDebugEvent(ev.dwProcessId, ev.dwThreadId, cont);
        if (r != -2) return r;
    }
    return -1;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 16];
    STARTUPINFOA si = {sizeof(si)};
    PROCESS_INFORMATION pi;

    if (argc > 1) { Sleep(120000); return 0; }
    snprintf(cmd, sizeof(cmd), "\"%s\" child", argv[0]);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn %lu\n", GetLastError());
        return 1;
    }
    Sleep(1000);
    check(DebugActiveProcess(pi.dwProcessId), "a debugger attaches");
    check(until_breakpoint() == 1, "the attach's break-in reaches the debugger");
    check(DebugBreakProcess(pi.hProcess), "DebugBreakProcess");
    check(until_breakpoint() == 1, "that break-in reaches the debugger");
    check(DebugActiveProcessStop(pi.dwProcessId), "the debugger detaches");
    TerminateProcess(pi.hProcess, 0);
    printf("%d failures\n", failures);
    return 0;
}
