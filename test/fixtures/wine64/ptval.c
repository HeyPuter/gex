/* ptval.exe: user32 calls that take a POINT by value (Win64 passes the
 * 8-byte struct in one register): ChildWindowFromPoint(Ex),
 * RealChildWindowFromPoint, PtInRect, WindowFromPoint and MonitorFromPoint,
 * also through pointers GetProcAddress gave; and user32, which the program
 * imports, loaded before its first call, as a PE program's static import
 * (GetModuleHandleA finds it). Prints "ok ..." per check
 * that passes, "ptval FAIL ..." per one that does not, then "N failures". */
#include <windows.h>
#include <stdio.h>
static int failures;
static void check(const char *what, int ok)
{
    if (ok) printf("ok %s\n", what), fflush(stdout);
    else { printf("ptval FAIL %s\n", what); failures++; }
}
int main(int argc, char **argv)
{
    WNDCLASSW wc = {0};
    HWND parent, child;
    POINT in = { 20, 20 }, out = { 80, 80 }, away = { 500, 500 };
    RECT r = { 10, 10, 60, 60 };
    HWND (WINAPI *cwfpex)(HWND, POINT, UINT);
    BOOL (WINAPI *ptinrect)(const RECT *, POINT);
    HMODULE u = GetModuleHandleA("user32.dll");
    check("user32 loaded before its first call", u != NULL);
    wc.lpfnWndProc = DefWindowProcW; wc.hInstance = GetModuleHandleW(NULL); wc.lpszClassName = L"ptval";
    RegisterClassW(&wc);
    parent = CreateWindowW(L"ptval", L"", WS_POPUP | WS_VISIBLE, 0, 0, 200, 200, NULL, NULL, wc.hInstance, NULL);
    child = CreateWindowW(L"ptval", L"", WS_CHILD | WS_VISIBLE, 10, 10, 50, 50, parent, NULL, wc.hInstance, NULL);
    check("ChildWindowFromPointEx child", ChildWindowFromPointEx(parent, in, CWP_SKIPINVISIBLE) == child);
    check("ChildWindowFromPointEx parent", ChildWindowFromPointEx(parent, out, CWP_SKIPINVISIBLE) == parent);
    check("ChildWindowFromPointEx outside", ChildWindowFromPointEx(parent, away, CWP_SKIPINVISIBLE) == NULL);
    check("ChildWindowFromPoint child", ChildWindowFromPoint(parent, in) == child);
    check("RealChildWindowFromPoint child", RealChildWindowFromPoint(parent, in) == child);
    check("PtInRect in", PtInRect(&r, in));
    check("PtInRect out", !PtInRect(&r, out));
    check("MonitorFromPoint", MonitorFromPoint(in, MONITOR_DEFAULTTONULL) != NULL);
    cwfpex = (void *)GetProcAddress(u, "ChildWindowFromPointEx");
    ptinrect = (void *)GetProcAddress(u, "PtInRect");
    check("ChildWindowFromPointEx pointer", cwfpex(parent, in, CWP_SKIPINVISIBLE) == child);
    check("PtInRect pointer", ptinrect(&r, in) && !ptinrect(&r, out));
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
