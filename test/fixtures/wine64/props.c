/* A file's Properties (its context menu's "properties" verb) opens shell32's
 * property sheet in a program whose activation context names comctl32 6, as
 * explorer's does (props.rc's manifest): shell32's PropertySheetW reached a
 * comctl32 never attached in the process, which looked for its dialog
 * template in the program's own image and showed nothing. */
#define COBJMACROS
#include <windows.h>
#include <shlobj.h>
#include <stdio.h>

static int failures;
static volatile LONG seen;

static void ok(int cond, const char *what)
{
    printf("%s props %s\n", cond ? "ok  " : "FAIL", what);
    if (!cond) failures++;
}

static BOOL CALLBACK find_sheet(HWND hwnd, LPARAM lp)
{
    WCHAR title[128];

    if (GetWindowTextW(hwnd, title, ARRAYSIZE(title)) && wcsstr(title, L"props.txt") && IsWindowVisible(hwnd))
    {
        InterlockedExchange(&seen, 1);
        PostMessageW(hwnd, WM_COMMAND, IDCANCEL, 0);
        return FALSE;
    }
    return TRUE;
}

/* Closes the sheet once it shows (the verb runs it modally on this thread). */
static DWORD WINAPI watch(void *arg)
{
    DWORD i;

    for (i = 0; i < 300 && !seen; i++)
    {
        EnumWindows(find_sheet, 0);
        Sleep(100);
    }
    return 0;
}

int main(void)
{
    IShellFolder *desktop, *folder;
    IContextMenu *menu;
    LPITEMIDLIST dir, item;
    CMINVOKECOMMANDINFO ci = { sizeof(ci) };
    HANDLE f, thread;
    HRESULT hr;

    setvbuf(stdout, NULL, _IONBF, 0);
    CoInitialize(NULL);
    f = CreateFileW(L"C:\\props.txt", GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, 0, NULL);
    WriteFile(f, "hi", 2, NULL, NULL);
    CloseHandle(f);
    hr = SHGetDesktopFolder(&desktop);
    ok(hr == S_OK, "the desktop folder");
    if (hr != S_OK) return 1;
    hr = IShellFolder_ParseDisplayName(desktop, NULL, NULL, (WCHAR *)L"C:\\", NULL, &dir, NULL);
    ok(hr == S_OK, "C:\\ parsed");
    if (hr != S_OK) return 1;
    hr = IShellFolder_BindToObject(desktop, dir, NULL, &IID_IShellFolder, (void **)&folder);
    ok(hr == S_OK, "C:\\ bound");
    if (hr != S_OK) return 1;
    hr = IShellFolder_ParseDisplayName(folder, NULL, NULL, (WCHAR *)L"props.txt", NULL, &item, NULL);
    ok(hr == S_OK, "props.txt parsed");
    if (hr != S_OK) return 1;
    hr = IShellFolder_GetUIObjectOf(folder, NULL, 1, (LPCITEMIDLIST *)&item, &IID_IContextMenu, NULL, (void **)&menu);
    ok(hr == S_OK, "the file's context menu");
    if (hr != S_OK) return 1;
    thread = CreateThread(NULL, 0, watch, NULL, 0, NULL);
    ci.lpVerb = "properties";
    ci.nShow = SW_SHOWNORMAL;
    IContextMenu_InvokeCommand(menu, &ci);
    WaitForSingleObject(thread, 35000);
    ok(seen, "the property sheet showed");
    printf("%d failures\n", failures);
    return failures;
}
