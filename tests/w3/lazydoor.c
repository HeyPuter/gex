/* A lazy file the page fetches slowly, mapped as an image: the wineserver
 * reads its headers inside the door call that makes the mapping, under the
 * lock every process's server calls take. Another process's server calls
 * must not wait for that fetch. The parent maps C:\lazy.dll (lazydoor.sh
 * makes it a lazy file whose every fetch takes GEX_LAZY_DELAY_MS) while a
 * child makes server calls as fast as it can and times the slowest. Prints
 * "lazydoor ok MAP_MS SLOWEST_MS", or "lazydoor FAIL ..." when the child's
 * slowest call waited as long as a fetch.
 *
 *   lazydoor.exe [DELAY_MS]   the parent (DELAY_MS: the fetch delay, 3000)
 *   lazydoor.exe -c           the child */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static double now_ms(void)
{
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c);
    QueryPerformanceFrequency(&f);
    return c.QuadPart * 1000.0 / f.QuadPart;
}

static int child(void)
{
    HANDLE ready = OpenEventA(EVENT_ALL_ACCESS, FALSE, "lazydoor-ready");
    HANDLE go = OpenEventA(EVENT_ALL_ACCESS, FALSE, "lazydoor-go");
    HANDLE stop = OpenEventA(EVENT_ALL_ACCESS, FALSE, "lazydoor-stop");
    double slowest = 0;

    if (!ready || !go || !stop) return 1000000;
    SetEvent(ready);
    WaitForSingleObject(go, INFINITE);
    /* each round trips to the server three times (create, set, close) */
    while (WaitForSingleObject(stop, 0) == WAIT_TIMEOUT)
    {
        double t = now_ms();
        HANDLE ev = CreateEventA(NULL, FALSE, FALSE, NULL);
        SetEvent(ev);
        CloseHandle(ev);
        t = now_ms() - t;
        if (t > slowest) slowest = t;
    }
    return (int)slowest;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 16], self[MAX_PATH];
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    HANDLE ready, go, stop, file, map;
    DWORD slowest;
    double t;
    int delay;

    if (argc > 1 && !strcmp(argv[1], "-c")) return child();
    delay = argc > 1 ? atoi(argv[1]) : 3000;
    ready = CreateEventA(NULL, TRUE, FALSE, "lazydoor-ready");
    go = CreateEventA(NULL, TRUE, FALSE, "lazydoor-go");
    stop = CreateEventA(NULL, TRUE, FALSE, "lazydoor-stop");
    GetModuleFileNameA(NULL, self, sizeof(self));
    sprintf(cmd, "\"%s\" -c", self);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        printf("lazydoor FAIL CreateProcess %lu\n", GetLastError());
        return 1;
    }
    if (WaitForSingleObject(ready, 120000))
    {
        printf("lazydoor FAIL the child never started\n");
        return 1;
    }
    file = CreateFileA("C:\\lazy.dll", GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
    if (file == INVALID_HANDLE_VALUE)
    {
        printf("lazydoor FAIL CreateFile %lu\n", GetLastError());
        return 1;
    }
    SetEvent(go);
    Sleep(200);
    t = now_ms();
    map = CreateFileMappingA(file, NULL, PAGE_READONLY | SEC_IMAGE, 0, 0, NULL);
    t = now_ms() - t;
    Sleep(200);
    SetEvent(stop);
    WaitForSingleObject(pi.hProcess, INFINITE);
    GetExitCodeProcess(pi.hProcess, &slowest);
    if (!map)
    {
        printf("lazydoor FAIL CreateFileMapping %lu\n", GetLastError());
        return 1;
    }
    /* the mapping waited for a fetch (the file is lazy and slow), and the
     * child's calls did not */
    if (t < delay * 0.8 || slowest >= (DWORD)delay / 2)
    {
        printf("lazydoor FAIL mapping %.0f ms, the child's slowest server call %lu ms (fetch delay %d ms)\n", t, slowest, delay);
        return 1;
    }
    printf("lazydoor ok %.0f %lu\n", t, slowest);
    return 0;
}
