/* remotestart: threads started in another process, a copy of this program
 * waiting on an event. A start below the native-function floor (a native
 * function's value, meaningful only in its own process) is refused
 * (ERROR_INVALID_PARAMETER); a guest address runs (kernel32's ExitThread,
 * exit code 0x2a). Then a debugger attaches (DebugActiveProcess, whose
 * break-in thread starts at ntdll's DbgUiRemoteBreakin) and breaks in again
 * (DebugBreakProcess): each break-in must reach the debugger as a
 * breakpoint. Told to go on, the child runs a ud2 in its own code: the
 * debugger must see that illegal instruction first (first chance), pass it
 * on and detach, and the child's vectored handler must then recover it
 * (exit 0x50; 0x51 when the handler never ran). Each check prints one line; "remotestart done, N failed"
 * last. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int fails;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    fflush(stdout);
    fails += !ok;
}

/* A ud2, and the instruction after it, where the child's handler resumes. */
void av_site(void);
extern char av_after[];
__asm__(".globl _av_site\n_av_site:\n\tud2\n.globl _av_after\n_av_after:\n\tret\n");

static int recovered;
static LONG CALLBACK recover(EXCEPTION_POINTERS *e)
{
    if (e->ExceptionRecord->ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION ||
        e->ContextRecord->Eip != (DWORD)av_site) return EXCEPTION_CONTINUE_SEARCH;
    e->ContextRecord->Eip = (DWORD)av_after;
    recovered = 1;
    return EXCEPTION_CONTINUE_EXECUTION;
}

/* The next exception's debug event, left uncontinued: 1 when it is a
 * first-chance illegal instruction, 0 for another exception, -1 for none. */
static int until_exception(DEBUG_EVENT *ev)
{
    while (WaitForDebugEvent(ev, 30000)) {
        if (ev->dwDebugEventCode == EXCEPTION_DEBUG_EVENT)
            return ev->u.Exception.ExceptionRecord.ExceptionCode == EXCEPTION_ILLEGAL_INSTRUCTION &&
                   ev->u.Exception.dwFirstChance;
        if (ev->dwDebugEventCode == LOAD_DLL_DEBUG_EVENT && ev->u.LoadDll.hFile) CloseHandle(ev->u.LoadDll.hFile);
        if (ev->dwDebugEventCode == EXIT_PROCESS_DEBUG_EVENT) return -1;
        ContinueDebugEvent(ev->dwProcessId, ev->dwThreadId, DBG_CONTINUE);
    }
    return -1;
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
    char cmd[MAX_PATH + 64], ev[32];
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    HANDLE go, t;
    DEBUG_EVENT dev;
    int r;
    DWORD code = 0;
    BOOL wow64 = FALSE;

    if (argc > 2 && !strcmp(argv[1], "child")) {
        HANDLE e = OpenEventA(SYNCHRONIZE, FALSE, argv[2]);
        if (!e) return 2;
        WaitForSingleObject(e, 120000);
        AddVectoredExceptionHandler(1, recover);
        av_site();
        return recovered ? 0x50 : 0x51;
    }
    sprintf(ev, "remotestart-%lu", GetCurrentProcessId());
    go = CreateEventA(NULL, TRUE, FALSE, ev);
    sprintf(cmd, "\"%s\" child %s", argv[0], ev);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn %lu\n", GetLastError());
        return 1;
    }
    Sleep(1000);
    /* Not under WoW64 with a translated 32-bit side: there it has no
     * native functions, and 0x1234 is a guest address like any other, whose
     * thread faults and takes the child with it, as on Windows. Under WoW64
     * with native i386 Wine ("native32", test/wow64/procvm) it has. */
    IsWow64Process(GetCurrentProcess(), &wow64);
    if (!wow64 || (argc > 1 && !strcmp(argv[1], "native32"))) {
        SetLastError(0);
        t = CreateRemoteThread(pi.hProcess, NULL, 0, (LPTHREAD_START_ROUTINE)0x1234, NULL, 0, NULL);
        check(!t && GetLastError() == ERROR_INVALID_PARAMETER, "a native start for another process is refused");
        if (t) CloseHandle(t);
    }
    t = CreateRemoteThread(pi.hProcess, NULL, 0,
                           (LPTHREAD_START_ROUTINE)GetProcAddress(GetModuleHandleA("kernel32.dll"), "ExitThread"),
                           (void *)0x2a, 0, NULL);
    check(t != NULL, "a guest start for another process runs");
    if (t) {
        WaitForSingleObject(t, 30000);
        GetExitCodeThread(t, &code);
        check(code == 0x2a, "its exit code");
        CloseHandle(t);
    }
    check(DebugActiveProcess(pi.dwProcessId), "a debugger attaches");
    check(until_breakpoint() == 1, "the attach's break-in reaches the debugger");
    check(DebugBreakProcess(pi.hProcess), "DebugBreakProcess");
    check(until_breakpoint() == 1, "that break-in reaches the debugger");
    SetEvent(go);
    r = until_exception(&dev);
    check(r == 1, "the debugger sees the child's fault first");
    if (r >= 0) ContinueDebugEvent(dev.dwProcessId, dev.dwThreadId, DBG_EXCEPTION_NOT_HANDLED);
    check(DebugActiveProcessStop(pi.dwProcessId), "the debugger detaches");
    WaitForSingleObject(pi.hProcess, 60000);
    GetExitCodeProcess(pi.hProcess, &code);
    printf("child exit %lx\n", code);
    check(code == 0x50, "the child recovers and goes on");
    printf("remotestart done, %d failed\n", fails);
    return fails != 0;
}
