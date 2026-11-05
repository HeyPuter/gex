/* A folder view shows a file created in its folder once the shell is told
 * (SHChangeNotify(SHCNE_CREATE, SHCNF_PATHW)), as the file manager's paste
 * does: for a folder under My Computer (C:\shellnotify) and for one the
 * desktop names as a special folder (Documents, CSIDL_PERSONAL), whose view
 * registers for its notifications under that folder's own pidl.
 * gex-nt-arch: x64
 * gex-nt-libs: -lshell32 -lole32 -luuid
 */
#define COBJMACROS
#include <windows.h>
#include <shlobj.h>
#include <stdio.h>

static int failures;

static void ok(int cond, const char *what, int before, int after)
{
    printf("%s shellnotify %s: %d item(s) before, %d after\n", cond ? "ok  " : "FAIL", what, before, after);
    if (!cond) failures++;
}

static int count_items(HWND view)
{
    HWND list = FindWindowExW(view, NULL, L"SysListView32", NULL);
    return list ? (int)SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) : -1;
}

/* The least shell browser a view takes (CreateViewWindow wants an owner). */
static HWND browser_hwnd;
static HRESULT WINAPI sb_qi(IShellBrowser *iface, REFIID riid, void **out)
{
    if (IsEqualIID(riid, &IID_IUnknown) || IsEqualIID(riid, &IID_IOleWindow) || IsEqualIID(riid, &IID_IShellBrowser))
    {
        *out = iface;
        return S_OK;
    }
    *out = NULL;
    return E_NOINTERFACE;
}
static ULONG WINAPI sb_addref(IShellBrowser *iface) { return 2; }
static ULONG WINAPI sb_release(IShellBrowser *iface) { return 1; }
static HRESULT WINAPI sb_getwindow(IShellBrowser *iface, HWND *hwnd) { *hwnd = browser_hwnd; return S_OK; }
static HRESULT WINAPI sb_csh(IShellBrowser *iface, BOOL mode) { return E_NOTIMPL; }
static HRESULT WINAPI sb_insertmenus(IShellBrowser *iface, HMENU m, OLEMENUGROUPWIDTHS *w) { return E_NOTIMPL; }
static HRESULT WINAPI sb_setmenu(IShellBrowser *iface, HMENU m, HOLEMENU h, HWND w) { return E_NOTIMPL; }
static HRESULT WINAPI sb_removemenus(IShellBrowser *iface, HMENU m) { return E_NOTIMPL; }
static HRESULT WINAPI sb_setstatus(IShellBrowser *iface, LPCOLESTR t) { return E_NOTIMPL; }
static HRESULT WINAPI sb_modeless(IShellBrowser *iface, BOOL e) { return E_NOTIMPL; }
static HRESULT WINAPI sb_accel(IShellBrowser *iface, MSG *m, WORD id) { return E_NOTIMPL; }
static HRESULT WINAPI sb_browse(IShellBrowser *iface, LPCITEMIDLIST p, UINT f) { return E_NOTIMPL; }
static HRESULT WINAPI sb_stream(IShellBrowser *iface, DWORD m, IStream **s) { *s = NULL; return E_NOTIMPL; }
static HRESULT WINAPI sb_control(IShellBrowser *iface, UINT id, HWND *w) { *w = NULL; return E_NOTIMPL; }
static HRESULT WINAPI sb_sendcontrol(IShellBrowser *iface, UINT id, UINT msg, WPARAM wp, LPARAM lp, LRESULT *r) { if (r) *r = 0; return E_NOTIMPL; }
static HRESULT WINAPI sb_active(IShellBrowser *iface, IShellView **v) { *v = NULL; return E_NOTIMPL; }
static HRESULT WINAPI sb_onactive(IShellBrowser *iface, IShellView *v) { return S_OK; }
static HRESULT WINAPI sb_toolbar(IShellBrowser *iface, LPTBBUTTONSB b, UINT n, UINT f) { return E_NOTIMPL; }
static IShellBrowserVtbl sb_vtbl = { sb_qi, sb_addref, sb_release, sb_getwindow, sb_csh, sb_insertmenus, sb_setmenu,
    sb_removemenus, sb_setstatus, sb_modeless, sb_accel, sb_browse, sb_stream, sb_control, sb_sendcontrol, sb_active,
    sb_onactive, sb_toolbar };
static IShellBrowser browser = { &sb_vtbl };

static void pump(void)
{
    MSG msg;
    DWORD end = GetTickCount() + 500;
    while ((LONG)(end - GetTickCount()) > 0)
    {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
        Sleep(10);
    }
}

/* A view of pidl's folder in parent; a file made in dir and announced. */
static void one(HWND parent, LPITEMIDLIST pidl, const WCHAR *dir, const char *what)
{
    IShellFolder *desktop, *folder;
    IShellView *view;
    FOLDERSETTINGS fs = { FVM_DETAILS, 0 };
    RECT rc = { 0, 0, 400, 300 };
    HWND hview = NULL;
    WCHAR file[MAX_PATH];
    HANDLE h;
    int before, after;

    SHGetDesktopFolder(&desktop);
    if (FAILED(IShellFolder_BindToObject(desktop, pidl, NULL, &IID_IShellFolder, (void **)&folder)))
    {
        printf("FAIL shellnotify %s: no folder\n", what); failures++; return;
    }
    if (FAILED(IShellFolder_CreateViewObject(folder, parent, &IID_IShellView, (void **)&view)) ||
        FAILED(IShellView_CreateViewWindow(view, NULL, &fs, &browser, &rc, &hview)) || !hview)
    {
        printf("FAIL shellnotify %s: no view\n", what); failures++; return;
    }
    pump();
    before = count_items(hview);
    swprintf(file, MAX_PATH, L"%ls\\shellnotify-%lu.txt", dir, GetCurrentProcessId());
    h = CreateFileW(file, GENERIC_WRITE, 0, NULL, CREATE_NEW, 0, NULL);
    if (h == INVALID_HANDLE_VALUE)
    {
        printf("FAIL shellnotify %s: CreateFileW %lu\n", what, GetLastError()); failures++; return;
    }
    CloseHandle(h);
    SHChangeNotify(SHCNE_CREATE, SHCNF_PATHW, file, NULL);
    pump();
    after = count_items(hview);
    ok(before >= 0 && after == before + 1, what, before, after);
    IShellView_DestroyViewWindow(view);
    IShellView_Release(view);
    IShellFolder_Release(folder);
    IShellFolder_Release(desktop);
    DeleteFileW(file);
}

int main(int argc, char **argv)
{
    WCHAR docs[MAX_PATH];
    LPITEMIDLIST pidl;
    HWND parent;
    HRESULT hr, hr2 = E_FAIL;

    OleInitialize(NULL);
    parent = browser_hwnd = CreateWindowW(L"static", L"shellnotify", WS_OVERLAPPEDWINDOW, 0, 0, 500, 400, NULL, NULL, NULL, NULL);

    /* the profile's folders, for a prefix made for another user (the shell's
     * desktop folder needs its Desktop) */
    SHGetFolderPathW(NULL, CSIDL_DESKTOPDIRECTORY | CSIDL_FLAG_CREATE, NULL, 0, docs);
    SHGetFolderPathW(NULL, CSIDL_PERSONAL | CSIDL_FLAG_CREATE, NULL, 0, docs);
    CreateDirectoryW(L"C:\\shellnotify", NULL);
    if (SUCCEEDED(hr = SHParseDisplayName(L"C:\\shellnotify", NULL, &pidl, 0, NULL)))
    {
        one(parent, pidl, L"C:\\shellnotify", "C:\\shellnotify");
        ILFree(pidl);
    }
    else { printf("FAIL shellnotify: no pidl for C:\\shellnotify (%#lx)\n", hr); failures++; }

    if (SUCCEEDED(hr = SHGetFolderLocation(NULL, CSIDL_PERSONAL, NULL, 0, &pidl)) &&
        SUCCEEDED(hr2 = SHGetFolderPathW(NULL, CSIDL_PERSONAL, NULL, 0, docs)))
    {
        CreateDirectoryW(docs, NULL);
        one(parent, pidl, docs, "Documents (CSIDL_PERSONAL)");
        ILFree(pidl);
    }
    else { printf("FAIL shellnotify: no Documents (%#lx, %#lx)\n", hr, hr2); failures++; }

    DestroyWindow(parent);
    OleUninitialize();
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
