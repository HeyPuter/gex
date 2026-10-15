/* The desktop as a thread (WINEGEX_DESKTOP_THREAD=1, tests/w3/desktopthread.sh):
 * the first window the program asks for starts explorer's desktop as a
 * thread of this process, and no explorer process runs beside it.
 * win32u's get_desktop_window went on to create an "explorer.exe /desktop"
 * process after starting the thread, suspended, and never resumed or closed
 * it: a fork for nothing in every desktop-thread session, and on 2026-10-14
 * the child the nt-trampolines trap was first seen in.
 *
 * Prints "desktopthread ok" when the desktop window exists and no
 * explorer.exe process is in the session. */
#include <windows.h>
#include <tlhelp32.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    HWND d = GetDesktopWindow();
    PROCESSENTRY32 pe;
    HANDLE snap;
    int explorers = 0, procs = 0;

    if (!d) { printf("desktopthread: no desktop window\n"); return 1; }
    snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) { printf("desktopthread: no snapshot: %lu\n", GetLastError()); return 1; }
    pe.dwSize = sizeof pe;
    for (BOOL more = Process32First(snap, &pe); more; more = Process32Next(snap, &pe))
    {
        procs++;
        printf("process %lu %s (%lu threads)\n", pe.th32ProcessID, pe.szExeFile, pe.cntThreads);
        if (!_stricmp(pe.szExeFile, "explorer.exe")) explorers++;
    }
    CloseHandle(snap);
    if (explorers) { printf("desktopthread: %d explorer.exe process(es) beside the desktop thread\n", explorers); return 1; }
    printf("desktopthread ok (%d processes)\n", procs);
    return 0;
}
