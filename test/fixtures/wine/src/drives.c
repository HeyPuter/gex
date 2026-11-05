/* The prefix's drives are listed (GetLogicalDrives, QueryDosDevice) in a
 * session that starts no services (WINEGEX_NO_WINEBOOT=1, the page's boot),
 * where no mountmgr makes their \??\X: links: the file manager's My Computer
 * listed none. The links name volumes no device serves there: C:'s volume
 * information, free space and type still come (ntdll asks the file system),
 * and My Computer, the folder the file dialogs list, holds C:. The test
 * prefix has c: and z:.
 * gex-nt-arch: x64
 * gex-nt-libs: -lshell32 -lole32 -luuid
 */
#define COBJMACROS
#include <windows.h>
#include <shlobj.h>
#include <stdio.h>

static int failures;

static void ok(int cond, const char *what)
{
    printf("%s drives %s\n", cond ? "ok  " : "FAIL", what);
    if (!cond) failures++;
}

int main(void)
{
    DWORD drives = GetLogicalDrives();
    WCHAR target[MAX_PATH], strings[256], *s;
    int n = 0, has_c = 0;

    ok((drives & (1 << 2)) != 0, "C: in GetLogicalDrives");
    ok((drives & (1 << 25)) != 0, "Z: in GetLogicalDrives");
    ok(QueryDosDeviceW(L"C:", target, MAX_PATH) && target[0] == '\\', "C: has a device");
    ok(GetDriveTypeW(L"C:\\") == DRIVE_FIXED, "C: is a fixed drive");
    if (GetLogicalDriveStringsW(ARRAYSIZE(strings), strings))
        for (s = strings; *s; s += lstrlenW(s) + 1, n++)
            if (!lstrcmpiW(s, L"C:\\")) has_c = 1;
    ok(has_c && n >= 2, "GetLogicalDriveStrings lists C:\\ and Z:\\");
    {
        WCHAR label[MAX_PATH], fs[MAX_PATH];
        DWORD serial, maxlen, flags;
        ULARGE_INTEGER avail, total, free;

        ok(GetVolumeInformationW(L"C:\\", label, MAX_PATH, &serial, &maxlen, &flags, fs, MAX_PATH) &&
           maxlen > 0 && fs[0], "C:'s volume information");
        ok(GetDiskFreeSpaceExW(L"C:\\", &avail, &total, &free) && total.QuadPart > 0, "C:'s free space");
    }
    {
        IShellFolder *desktop, *drives;
        IEnumIDList *items;
        LPITEMIDLIST pidl, item;
        WCHAR name[MAX_PATH];
        int found = 0;

        CoInitialize(NULL);
        if (SUCCEEDED(SHGetDesktopFolder(&desktop)) &&
            SUCCEEDED(SHGetSpecialFolderLocation(NULL, CSIDL_DRIVES, &pidl)) &&
            SUCCEEDED(IShellFolder_BindToObject(desktop, pidl, NULL, &IID_IShellFolder, (void **)&drives)) &&
            IShellFolder_EnumObjects(drives, NULL, SHCONTF_FOLDERS, &items) == S_OK)
        {
            while (IEnumIDList_Next(items, 1, &item, NULL) == S_OK)
            {
                LPITEMIDLIST full = ILCombine(pidl, item);

                if (full && SHGetPathFromIDListW(full, name) && !lstrcmpiW(name, L"C:\\")) found = 1;
                ILFree(full);
                CoTaskMemFree(item);
            }
            IEnumIDList_Release(items);
        }
        ok(found, "My Computer lists C:");
    }
    printf("%d failures\n", failures);
    return failures;
}
