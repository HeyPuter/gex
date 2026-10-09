/* attachwin's DLL: its DllMain makes a window (which needs the desktop)
 * while the loader lock is held, as some games' DLLs and drivers do.
 * gex-nt-dll: 0x1a000000 */
#include <windows.h>
__declspec(dllexport) HWND attachwin_hwnd;
static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcA(h, m, w, l); }
BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, void *reserved) {
    if (reason == DLL_PROCESS_ATTACH) {
        WNDCLASSA c = {0};
        c.lpfnWndProc = proc; c.hInstance = inst; c.lpszClassName = "attachwin";
        RegisterClassA(&c);
        if (GetDesktopWindow())
            attachwin_hwnd = CreateWindowA("attachwin", "attachwin", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, 0, 0, inst, 0);
    }
    return TRUE;
}
