/* dbgflag: a debugger's two ways in, and the flag each must leave in the
 * debuggee's PEB. "spawn": CreateProcess with DEBUG_ONLY_THIS_PROCESS;
 * "attach": DebugActiveProcess on a running child. The child exits with
 * 0x61 when IsDebuggerPresent says so, 0x60 when not. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int child(const char *ev)
{
    if (ev) {
        HANDLE go = OpenEventA(SYNCHRONIZE, FALSE, ev);
        if (!go) return 2;
        WaitForSingleObject(go, 120000);
    }
    return IsDebuggerPresent() ? 0x61 : 0x60;
}

/* Run the debug loop until the debuggee exits; its exit code. */
static DWORD debug_loop(void)
{
    DEBUG_EVENT de;
    for (;;) {
        DWORD cont = DBG_CONTINUE;
        if (!WaitForDebugEvent(&de, 120000)) return 0xdead;
        if (de.dwDebugEventCode == EXCEPTION_DEBUG_EVENT &&
            de.u.Exception.ExceptionRecord.ExceptionCode != EXCEPTION_BREAKPOINT)
            cont = DBG_EXCEPTION_NOT_HANDLED;
        if (de.dwDebugEventCode == CREATE_PROCESS_DEBUG_EVENT && de.u.CreateProcessInfo.hFile)
            CloseHandle(de.u.CreateProcessInfo.hFile);
        if (de.dwDebugEventCode == LOAD_DLL_DEBUG_EVENT && de.u.LoadDll.hFile)
            CloseHandle(de.u.LoadDll.hFile);
        if (de.dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT) {
            ContinueDebugEvent(de.dwProcessId, de.dwThreadId, DBG_CONTINUE);
            return de.u.ExitProcess.dwExitCode;
        }
        ContinueDebugEvent(de.dwProcessId, de.dwThreadId, cont);
    }
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 64], ev[32];
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    DWORD code;
    HANDLE go;
    int fails = 0;

    if (argc > 1 && !strcmp(argv[1], "child")) return child(argc > 2 ? argv[2] : NULL);

    sprintf(cmd, "\"%s\" child", argv[0]);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, DEBUG_ONLY_THIS_PROCESS, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn under a debugger: %lu\n", GetLastError());
        return 1;
    }
    code = debug_loop();
    printf("%s spawned under a debugger: child exit %02lx\n", code == 0x61 ? "ok  " : "FAIL", (unsigned long)code);
    fails += code != 0x61;
    fflush(stdout);

    sprintf(ev, "dbgflag-%lu", GetCurrentProcessId());
    go = CreateEventA(NULL, TRUE, FALSE, ev);
    sprintf(cmd, "\"%s\" child %s", argv[0], ev);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn %lu\n", GetLastError());
        return 1;
    }
    Sleep(1000);
    if (!DebugActiveProcess(pi.dwProcessId)) {
        printf("FAIL attach: error %lu\n", GetLastError());
        fails++;
        SetEvent(go);
        WaitForSingleObject(pi.hProcess, 120000);
    } else {
        SetEvent(go);
        code = debug_loop();
        printf("%s attached: child exit %02lx\n", code == 0x61 ? "ok  " : "FAIL", (unsigned long)code);
        fails += code != 0x61;
    }
    printf("dbgflag done, %d failed\n", fails);
    return fails;
}
