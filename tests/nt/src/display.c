/* What a fullscreen game asks of the system before it draws (Peggle's
 * SexyFramework and wined3d): the display devices and modes, a mode change
 * and the metrics that follow it, D3DKMT's adapter and a DC over the
 * caller's memory, memory status and the process's VM counters, a named
 * section mapped twice, and a dialog with its procedure.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 -lpsapi */
#include <windows.h>
#include <psapi.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

/* d3dkmthk.h's structures, which MinGW does not ship in every version. */
typedef UINT D3DKMT_HANDLE;
typedef struct { WCHAR DeviceName[32]; D3DKMT_HANDLE hAdapter; LUID AdapterLuid; UINT VidPnSourceId; } OPENADAPTER;
typedef struct { void *pMemory; UINT Format, Width, Height, Pitch; HDC hDeviceDc; PALETTEENTRY *pColorTable; HDC hDc; HANDLE hBitmap; } CREATEDC;
typedef struct { HDC hDc; HANDLE hBitmap; } DESTROYDC;
typedef struct { D3DKMT_HANDLE hAdapter; } CLOSEADAPTER;
typedef LONG (WINAPI *pfn)(void *);

static INT_PTR CALLBACK dlg_proc(HWND h, UINT msg, WPARAM wp, LPARAM lp) {
    (void)wp;
    if (msg == WM_INITDIALOG) {
        SetWindowLongPtrA(h, DWLP_USER, lp);
        return TRUE;
    }
    return FALSE;
}

int main(void) {
    DISPLAY_DEVICEA dd = {.cb = sizeof dd};
    CHECK("EnumDisplayDevices finds the adapter", EnumDisplayDevicesA(NULL, 0, &dd, 0) && (dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE));
    char adapter[32];
    strcpy(adapter, dd.DeviceName);
    DISPLAY_DEVICEA dd2 = {.cb = sizeof dd2};
    CHECK("and only the one adapter", !EnumDisplayDevicesA(NULL, 1, &dd2, 0));
    DISPLAY_DEVICEA mon = {.cb = sizeof mon};
    CHECK("its monitor", EnumDisplayDevicesA(adapter, 0, &mon, 0));

    DEVMODEA dm = {.dmSize = sizeof dm};
    int modes = 0, has800 = 0;
    for (DWORD i = 0; EnumDisplaySettingsA(NULL, i, &dm); i++) {
        modes++;
        if (dm.dmPelsWidth == 800 && dm.dmPelsHeight == 600 && dm.dmBitsPerPel == 32) has800 = 1;
    }
    CHECK("several modes, 800x600x32 among them", modes > 3 && has800);
    int w0 = GetSystemMetrics(SM_CXSCREEN), h0 = GetSystemMetrics(SM_CYSCREEN);

    DEVMODEA want = {.dmSize = sizeof want};
    want.dmPelsWidth = 800;
    want.dmPelsHeight = 600;
    want.dmBitsPerPel = 32;
    want.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
    CHECK("CDS_TEST of 800x600", ChangeDisplaySettingsA(&want, CDS_TEST) == DISP_CHANGE_SUCCESSFUL);
    CHECK("CDS_TEST leaves the mode", GetSystemMetrics(SM_CXSCREEN) == w0);
    want.dmPelsWidth = 801;
    CHECK("a mode not on offer is refused", ChangeDisplaySettingsA(&want, CDS_FULLSCREEN) == DISP_CHANGE_BADMODE);
    want.dmPelsWidth = 800;
    CHECK("change to 800x600", ChangeDisplaySettingsA(&want, CDS_FULLSCREEN) == DISP_CHANGE_SUCCESSFUL);
    CHECK("the screen metrics follow", GetSystemMetrics(SM_CXSCREEN) == 800 && GetSystemMetrics(SM_CYSCREEN) == 600);
    DEVMODEA cur = {.dmSize = sizeof cur};
    CHECK("ENUM_CURRENT_SETTINGS is the new mode", EnumDisplaySettingsA(NULL, ENUM_CURRENT_SETTINGS, &cur) && cur.dmPelsWidth == 800);
    RECT dr;
    GetWindowRect(GetDesktopWindow(), &dr);
    CHECK("the desktop window takes its size", dr.right == 800 && dr.bottom == 600);
    CHECK("restore", ChangeDisplaySettingsA(NULL, 0) == DISP_CHANGE_SUCCESSFUL);
    CHECK("the metrics are back", GetSystemMetrics(SM_CXSCREEN) == w0 && GetSystemMetrics(SM_CYSCREEN) == h0);

    HMODULE gdi = GetModuleHandleA("gdi32.dll");
    pfn open = (pfn)GetProcAddress(gdi, "D3DKMTOpenAdapterFromGdiDisplayName");
    pfn create = (pfn)GetProcAddress(gdi, "D3DKMTCreateDCFromMemory");
    pfn destroy = (pfn)GetProcAddress(gdi, "D3DKMTDestroyDCFromMemory");
    pfn close_ = (pfn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    OPENADAPTER oa = {0};
    MultiByteToWideChar(CP_ACP, 0, adapter, -1, oa.DeviceName, 32);
    CHECK("D3DKMTOpenAdapterFromGdiDisplayName", open && open(&oa) == 0 && oa.hAdapter != 0);
    static DWORD pixels[16 * 8];
    CREATEDC cd = {.pMemory = pixels, .Format = 21 /* A8R8G8B8 */, .Width = 16, .Height = 8, .Pitch = 64, .hDeviceDc = NULL};
    CHECK("D3DKMTCreateDCFromMemory", create && create(&cd) == 0 && cd.hDc && cd.hBitmap);
    if (cd.hDc) {
        RECT r = {2, 1, 4, 3};
        HBRUSH b = CreateSolidBrush(RGB(0x10, 0x20, 0x30));
        FillRect(cd.hDc, &r, b);
        DeleteObject(b);
        GdiFlush();
        CHECK("GDI draws into the caller's memory, top-down", pixels[1 * 16 + 2] == 0x102030 && pixels[0] == 0);
        DESTROYDC ds = {cd.hDc, cd.hBitmap};
        CHECK("D3DKMTDestroyDCFromMemory", destroy(&ds) == 0);
    }
    CLOSEADAPTER ca = {oa.hAdapter};
    CHECK("D3DKMTCloseAdapter", close_ && close_(&ca) == 0);

    MEMORYSTATUSEX ms = {.dwLength = sizeof ms};
    CHECK("GlobalMemoryStatusEx", GlobalMemoryStatusEx(&ms) && ms.ullTotalPhys > 0 && ms.ullAvailPhys > 0 && ms.ullAvailPhys <= ms.ullTotalPhys);
    PROCESS_MEMORY_COUNTERS pmc = {.cb = sizeof pmc};
    CHECK("GetProcessMemoryInfo", GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof pmc) && pmc.WorkingSetSize > 0 && pmc.PagefileUsage > 0);

    HANDLE s1 = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 4096, "gex-display-shared");
    HANDLE s2 = OpenFileMappingA(FILE_MAP_ALL_ACCESS, FALSE, "gex-display-shared");
    void **v1 = MapViewOfFile(s1, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    void **v2 = MapViewOfFile(s2, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    CHECK("a named section mapped twice", v1 && v2);
    if (v1 && v2) {
        v1[0] = (void *)main;
        CHECK("the views share their pages", v2[0] == (void *)main);
        UnmapViewOfFile(v1);
        CHECK("one stays mapped after the other's unmap", v2[0] == (void *)main);
        UnmapViewOfFile(v2);
    }
    CloseHandle(s1);
    CloseHandle(s2);

    /* A dialog from an in-memory template: one button. */
    struct { DLGTEMPLATE t; WORD menu, cls, title; } tpl = {{WS_POPUP, 0, 0, 0, 0, 100, 50}, 0, 0, 0};
    HWND dlg = CreateDialogIndirectParamA(GetModuleHandleA(NULL), &tpl.t, NULL, dlg_proc, 1234);
    CHECK("a dialog", dlg != NULL);
    CHECK("its procedure ran WM_INITDIALOG", dlg && GetWindowLongPtrA(dlg, DWLP_USER) == 1234);
    CHECK("DWLP_DLGPROC is a procedure", dlg && GetWindowLongPtrA(dlg, DWLP_DLGPROC) != 0);
    HWND btn = CreateWindowA("BUTTON", "b", WS_CHILD | BS_CHECKBOX, 0, 0, 10, 10, dlg, (HMENU)7, NULL, NULL);
    SendMessageA(btn, BM_SETCHECK, BST_CHECKED, 0);
    CHECK("a builtin control keeps its state", SendMessageA(btn, BM_GETCHECK, 0, 0) == BST_CHECKED);
    if (dlg) DestroyWindow(dlg);

    printf("%d failures\n", failures);
    return failures;
}
