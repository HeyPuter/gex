/* The work area one process sets (SPI_SETWORKAREA, explorer's taskbar) is
 * every process's: another process reads it (SPI_GETWORKAREA, the primary
 * monitor's rcWork) and its maximised window stops at it, not at the
 * screen's bottom edge under the taskbar.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>

#define TASKBAR 40

static int failures;

static void ok(int cond, const char *what, const RECT *r)
{
    printf("%s workarea %s: (%ld,%ld)-(%ld,%ld)\n", cond ? "ok  " : "FAIL", what,
           r->left, r->top, r->right, r->bottom);
    if (!cond) failures++;
}

static int child(void)
{
    int w = GetSystemMetrics(SM_CXSCREEN), h = GetSystemMetrics(SM_CYSCREEN);
    RECT want = { 0, 0, w, h - TASKBAR }, got, frame = { 0, 0, 0, 0 };
    MONITORINFO mi = { sizeof(mi) };
    HWND hwnd;
    MSG msg;

    SystemParametersInfoW(SPI_GETWORKAREA, 0, &got, 0);
    ok(EqualRect(&got, &want), "another process's SPI_GETWORKAREA", &got);
    GetMonitorInfoW(MonitorFromPoint((POINT){ 0, 0 }, MONITOR_DEFAULTTOPRIMARY), &mi);
    ok(EqualRect(&mi.rcWork, &want), "another process's monitor rcWork", &mi.rcWork);
    hwnd = CreateWindowExW(0, L"STATIC", L"workarea", WS_OVERLAPPEDWINDOW, 10, 10, 200, 100, 0, 0, 0, 0);
    ShowWindow(hwnd, SW_MAXIMIZE);
    while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
    GetWindowRect(hwnd, &got);
    /* the frame hangs over the work area's edges by its border width, as on Windows */
    AdjustWindowRectEx(&frame, WS_OVERLAPPEDWINDOW, FALSE, 0);
    ok(got.bottom <= want.bottom + frame.bottom && got.bottom > want.bottom - 8, "another process's maximised window", &got);
    DestroyWindow(hwnd);
    return failures;
}

int main(int argc, char **argv)
{
    int w = GetSystemMetrics(SM_CXSCREEN), h = GetSystemMetrics(SM_CYSCREEN);
    RECT work = { 0, 0, w, h - TASKBAR }, full = { 0, 0, w, h }, got;
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    char cmd[MAX_PATH + 16];
    DWORD code = 1;

    if (argc > 1 && !strcmp(argv[1], "child")) return child();
    ok(SystemParametersInfoW(SPI_SETWORKAREA, 0, &work, SPIF_SENDCHANGE), "SPI_SETWORKAREA", &work);
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &got, 0);
    ok(EqualRect(&got, &work), "this process's SPI_GETWORKAREA", &got);
    snprintf(cmd, sizeof(cmd), "\"%s\" child", argv[0]);
    fflush(stdout);
    if (CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi))
    {
        WaitForSingleObject(pi.hProcess, 60000);
        GetExitCodeProcess(pi.hProcess, &code);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    failures += code;
    SystemParametersInfoW(SPI_SETWORKAREA, 0, &full, SPIF_SENDCHANGE);
    printf("%d failures\n", failures);
    return failures;
}
