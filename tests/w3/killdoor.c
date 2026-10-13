/* A process killed while its threads are inside door calls (the page
 * ends a Worker with terminate()): the server must stay whole and the
 * session go on. The parent kills ROUNDS children that make server
 * requests as fast as they can, then makes requests itself. Prints
 * "killdoor ok ROUNDS". docs/wineserver-kernel.md §4.2 (the in-door word).
 *
 *   killdoor.exe [ROUNDS]   the parent (default 8)
 *   killdoor.exe -c         a child: requests from three threads, forever */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static DWORD WINAPI spin(void *arg)
{
    for (;;)
    {
        HANDLE ev = CreateEventA(NULL, FALSE, FALSE, NULL);
        SetEvent(ev);
        WaitForSingleObject(ev, 0);
        CloseHandle(ev);
    }
    return 0;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 16], self[MAX_PATH];
    int rounds = argc > 1 ? atoi(argv[1]) : 8, i;

    if (argc > 1 && !strcmp(argv[1], "-c"))
    {
        CreateThread(NULL, 0, spin, NULL, 0, NULL);
        CreateThread(NULL, 0, spin, NULL, 0, NULL);
        spin(NULL);
    }
    if (rounds < 1) rounds = 8;
    GetModuleFileNameA(NULL, self, sizeof(self));
    sprintf(cmd, "\"%s\" -c", self);
    for (i = 0; i < rounds; i++)
    {
        STARTUPINFOA si = { sizeof(si) };
        PROCESS_INFORMATION pi;
        if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) { printf("killdoor: no child %d\n", i); return 1; }
        Sleep(1500 + 97 * i);
        if (!TerminateProcess(pi.hProcess, 7)) { printf("killdoor: cannot kill child %d\n", i); return 1; }
        if (WaitForSingleObject(pi.hProcess, 30000) != WAIT_OBJECT_0) { printf("killdoor: child %d did not end\n", i); return 1; }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    for (i = 0; i < 1000; i++)
    {
        HANDLE ev = CreateEventA(NULL, FALSE, FALSE, NULL);
        if (!ev || !CloseHandle(ev)) { printf("killdoor: the server stopped answering\n"); return 1; }
    }
    printf("killdoor ok %d\n", rounds);
    return 0;
}
